//! What a user can ask for, independent of how the request is delivered.
//!
//! Options typed after a model prefix are parsed into [`Requested`] and checked
//! **before anything is sent**, in two layers:
//!
//! 1. the vendor's **official** capabilities ([`official`]) — what the model's
//!    own API offers (e.g. Gemini has code execution and URL context);
//! 2. the **route's** capabilities ([`RouteCaps`]) — what the path we call it
//!    through can actually deliver (OpenRouter today; the vendors' own APIs
//!    soon).
//!
//! Anything outside both is rejected locally; something the vendor offers but
//! the route cannot deliver is rejected with "<route> does not support …", so
//! nothing is ever left for the router to silently drop or "correct".
//!
//! The vocabulary is deliberately small: the old bot's flags plus a few the
//! project owner asked for. There is no free-form JSON.

use omni_core::trigger::OptionToken;

/// Built-in tools the provider executes itself (stateless).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// Web search (`s`).
    Search,
    /// Code execution (`c`).
    Code,
    /// Reading web pages / URLs given in the prompt (`u`).
    Fetch,
    /// Google Maps grounding (`m`); the conversation's latest location is the
    /// user's position.
    Maps,
    /// The provider's own image generation tool in a chat (`i`; OpenAI and
    /// xAI `image_generation`).
    Image,
}

impl Tool {
    pub fn letter(self) -> char {
        match self {
            Tool::Search => 's',
            Tool::Code => 'c',
            Tool::Fetch => 'u',
            Tool::Maps => 'm',
            Tool::Image => 'i',
        }
    }
    pub fn describe(self) -> &'static str {
        match self {
            Tool::Search => "web search",
            Tool::Code => "code execution",
            Tool::Fetch => "URL fetching",
            Tool::Maps => "Google Maps",
            Tool::Image => "image generation",
        }
    }
}

/// `+h` / `+l`: the model's highest or lowest reasoning level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// The highest level (or the largest budget) the official API offers.
    Highest,
    /// Reasoning off where it can be turned off, else the lowest level.
    Lowest,
}

/// Parsed options (what was asked for, not yet validated).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Requested {
    pub tools: Vec<Tool>,
    /// Reasoning effort level; `none` disables reasoning. Unset: the official
    /// default ([`resolve_reasoning`]).
    pub effort: Option<String>,
    /// `+h` / `+l` (settled into `effort` or `budget` by [`resolve_reasoning`]).
    pub level: Option<Level>,
    /// Reasoning token budget.
    pub budget: Option<u64>,
    pub verbosity: Option<String>,
    /// `1K` / `2K` / `4K`.
    pub image_size: Option<String>,
    pub aspect_ratio: Option<String>,
    /// Provider slugs (route-specific meaning).
    pub providers: Option<Vec<String>>,
    pub tier: Option<String>,
    /// Image quality (`q=`; image generation models).
    pub quality: Option<String>,
    /// Number of images (`n=`; image generation models).
    pub count: Option<u32>,
    /// Image background (`bg=`: auto, opaque, transparent).
    pub background: Option<String>,
    /// Video length in seconds (`sec=`; video generation models).
    pub seconds: Option<u32>,
}

fn set<T: PartialEq + std::fmt::Debug>(slot: &mut Option<T>, v: T, what: &str) -> Result<(), String> {
    match slot {
        Some(old) if *old != v => Err(format!("conflicting {what} options ({old:?} and {v:?})")),
        _ => {
            *slot = Some(v);
            Ok(())
        }
    }
}

/// Parse option tokens. Unknown options are errors (never ignored).
pub fn parse(tokens: &[OptionToken]) -> Result<Requested, String> {
    let mut r = Requested::default();
    for t in tokens {
        match t {
            OptionToken::Json(_) => return Err("JSON options are not supported; see /help options".into()),
            OptionToken::Flag(f) => {
                for c in f.chars() {
                    match c {
                        's' | 'c' | 'u' | 'm' | 'i' => {
                            let tool = match c {
                                's' => Tool::Search,
                                'c' => Tool::Code,
                                'u' => Tool::Fetch,
                                'm' => Tool::Maps,
                                _ => Tool::Image,
                            };
                            if !r.tools.contains(&tool) {
                                r.tools.push(tool);
                            }
                        }
                        'h' => set(&mut r.level, Level::Highest, "reasoning level")?,
                        'l' => set(&mut r.level, Level::Lowest, "reasoning level")?,
                        _ => return Err(format!("unknown option `{c}` in `+{f}`; see /help options")),
                    }
                }
            }
            OptionToken::KeyValue(k, v) => match k.as_str() {
                "e" => set(&mut r.effort, v.to_ascii_lowercase(), "effort")?,
                "budget" => {
                    let n: u64 =
                        v.parse().map_err(|_| format!("`budget=` needs a whole number of tokens, got `{v}`"))?;
                    set(&mut r.budget, n, "budget")?;
                }
                "v" => set(&mut r.verbosity, v.to_ascii_lowercase(), "verbosity")?,
                "size" => {
                    let size = v.to_ascii_uppercase();
                    if !matches!(size.as_str(), "1K" | "2K" | "4K") {
                        return Err(format!("`size=` takes 1K, 2K or 4K, got `{v}`"));
                    }
                    set(&mut r.image_size, size, "image size")?;
                }
                "ar" => {
                    let ok = v.split_once(':').is_some_and(|(a, b)| {
                        !a.is_empty()
                            && !b.is_empty()
                            && a.bytes().all(|c| c.is_ascii_digit())
                            && b.bytes().all(|c| c.is_ascii_digit())
                    });
                    if !ok {
                        return Err(format!("`ar=` needs a ratio like 21:9, got `{v}`"));
                    }
                    set(&mut r.aspect_ratio, v.clone(), "aspect ratio")?;
                }
                "p" => {
                    let slugs: Vec<String> =
                        v.split('|').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
                    if slugs.is_empty() {
                        return Err("`p=` needs a provider slug".into());
                    }
                    set(&mut r.providers, slugs, "provider")?;
                }
                "tier" => set(&mut r.tier, v.to_ascii_lowercase(), "tier")?,
                "q" => set(&mut r.quality, v.to_ascii_lowercase(), "quality")?,
                "bg" => set(&mut r.background, v.to_ascii_lowercase(), "background")?,
                "n" => {
                    let n: u32 = v
                        .parse()
                        .ok()
                        .filter(|n| *n >= 1)
                        .ok_or_else(|| format!("`n=` needs a number of images (1 or more), got `{v}`"))?;
                    set(&mut r.count, n, "image count")?;
                }
                "sec" => {
                    let n: u32 = v
                        .parse()
                        .ok()
                        .filter(|n| *n >= 1)
                        .ok_or_else(|| format!("`sec=` needs a whole number of seconds, got `{v}`"))?;
                    set(&mut r.seconds, n, "video length")?;
                }
                other => return Err(format!("unknown option `{other}=`; see /help options")),
            },
        }
    }
    if r.effort.is_some() && r.budget.is_some() {
        return Err("use either e= or budget=, not both".into());
    }
    if let Some(l) = r.level
        && (r.effort.is_some() || r.budget.is_some())
    {
        let flag = if l == Level::Highest { "+h" } else { "+l" };
        return Err(format!("{flag} sets the reasoning level already; leave out e= and budget="));
    }
    Ok(r)
}

/// The vendor's official capabilities for a model (whatever the route).
pub mod official {
    use super::Tool;

    /// Model author from an id like `google/gemini-4-carbon` (or the provider
    /// family of a direct-API id like `doubao-seed-…`).
    pub fn vendor(model: &str) -> &str {
        if let Some((a, _)) = model.split_once('/') {
            return a;
        }
        if model.starts_with("doubao") {
            return "bytedance";
        }
        ""
    }

    /// Whether the vendor's own API offers `tool` for this model family.
    /// `None` = unknown vendor (only the route's capabilities decide).
    pub fn has_tool(model: &str, tool: Tool) -> Option<bool> {
        let v = vendor(model);
        Some(match (v, tool) {
            ("google", Tool::Maps) => true, // googleMaps grounding (Gemini API)
            (_, Tool::Maps) => false,
            ("openai" | "x-ai", Tool::Image) => true, // image_generation (Responses)
            (_, Tool::Image) => false,
            ("openai", Tool::Search | Tool::Code) => true,
            ("openai", Tool::Fetch) => false,
            ("anthropic", _) => true, // web_search, code_execution, web_fetch
            ("google", _) => true,    // google_search, code_execution, url_context
            ("x-ai", Tool::Search | Tool::Code) => true,
            ("x-ai", Tool::Fetch) => false,
            ("deepseek" | "z-ai" | "moonshotai" | "bytedance-seed" | "bytedance", Tool::Search) => true,
            ("qwen", Tool::Search | Tool::Code | Tool::Fetch) => true, // web_search, code_interpreter, web_extractor
            ("deepseek" | "z-ai" | "moonshotai" | "bytedance-seed" | "bytedance", _) => false,
            _ => return None,
        })
    }

    /// Official verbosity levels, if the model family has the parameter.
    pub fn verbosity_levels(model: &str) -> Option<&'static [&'static str]> {
        (vendor(model) == "openai").then_some(&["low", "medium", "high"][..])
    }

    /// Official image output options: (sizes, aspect ratios), for image models.
    pub fn image_options(model: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
        const BASE: &[&str] = &["1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9"];
        const EXTENDED: &[&str] =
            &["1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9", "1:4", "4:1", "1:8", "8:1"];
        let m = model.split_once('/').map(|(_, m)| m).unwrap_or(model);
        if !model.starts_with("google/") {
            return None;
        }
        if m.starts_with("gemini-nano-banana-2") {
            Some((&["1K", "2K", "4K"], EXTENDED))
        } else if m.starts_with("gemini-3.1-flash-lite-image") {
            // The fastest image model: 1K only (Google's model page, 2026-11-12).
            Some((&["1K"], BASE))
        } else if m.starts_with("gemini-3.1-flash-image") {
            Some((&["512", "1K", "2K", "4K"], EXTENDED))
        } else if m.contains("image") || m.contains("nano-banana") {
            Some((&["1K", "2K", "4K"], BASE))
        } else {
            None
        }
    }

    /// Image input detail level the vendor offers at most (OpenAI only has the parameter).
    pub fn max_image_detail(model: &str) -> Option<&'static str> {
        let m = model.strip_prefix("openai/")?;
        let original = ["gpt-6-astra", "gpt-5.6", "gpt-5.5", "gpt-5.4"].iter().any(|p| m.starts_with(p));
        Some(if original { "original" } else { "high" })
    }
}

/// What a route (OpenRouter, a vendor's own API, …) can deliver for a model.
pub trait RouteCaps {
    /// Route name for messages, e.g. `OpenRouter`.
    fn name(&self) -> &str;
    fn tool(&self, tool: Tool) -> bool;
    /// Effort levels (highest first); `None` if the route exposes no effort selection.
    fn efforts(&self) -> Option<Vec<String>>;
    /// Whether reasoning can be turned off (`e=none`).
    fn can_disable_reasoning(&self) -> bool;
    /// Whether a reasoning token budget can be set.
    fn budget(&self) -> bool;
    fn verbosity(&self) -> bool;
    /// Image output with size/aspect options.
    fn image_options(&self) -> bool;
    /// Service tiers available for the chosen provider(s), excluding the default.
    fn tiers(&self) -> Vec<String>;
    /// Which image/video generation options (`q=`, `n=`, `bg=`, `sec=`) the
    /// route takes for the model; their values are checked by the provider.
    fn generation(&self) -> Generation {
        Generation::default()
    }
    /// The reasoning level the bot uses when none is asked for: the official
    /// default (the level the API applies when none is sent, or the one the
    /// vendor recommends turning on), sent explicitly. `None`: no named default
    /// (nothing is sent and the API decides) or no reasoning levels.
    fn default_effort(&self) -> Option<String> {
        None
    }
    /// The model reasons at a depth the route cannot change: `+h` and `+l`
    /// are accepted and change nothing.
    fn fixed_reasoning(&self) -> bool {
        false
    }
    /// The largest and smallest reasoning token budget, for `+h` and `+l` on
    /// models without effort levels.
    fn budget_range(&self) -> Option<(u64, u64)> {
        None
    }
}

/// Settle the reasoning level (owner's rule, 2026-11-13): `+h` is the highest
/// level the route offers (or the largest budget), `+l` turns reasoning off
/// where the route can, else the lowest level (or the smallest budget); with
/// neither and no `e=`/`budget=`, the official default
/// ([`RouteCaps::default_effort`]). Returns the note for a default that is not
/// the highest level. Call after [`validate`].
pub fn resolve_reasoning(model: &str, r: &mut Requested, route: &dyn RouteCaps) -> Result<Option<String>, String> {
    let levels = route.efforts().unwrap_or_default();
    let named: Vec<&String> = levels.iter().filter(|l| *l != "none").collect();
    let can_off = route.can_disable_reasoning() || levels.iter().any(|l| l == "none");
    let via = route.name();
    match r.level {
        Some(Level::Highest) => {
            if let Some(top) = named.first() {
                r.effort = Some((*top).clone());
            } else if let Some((max, _)) = route.budget_range() {
                r.budget = Some(max);
            } else if !route.fixed_reasoning() {
                return Err(format!("`{model}` has no reasoning level to raise (`+h`) via {via}"));
            }
            Ok(None)
        }
        Some(Level::Lowest) => {
            if can_off {
                r.effort = Some("none".into());
            } else if let Some(low) = named.last() {
                r.effort = Some((*low).clone());
            } else if let Some((_, min)) = route.budget_range() {
                r.budget = Some(min);
            } else if !route.fixed_reasoning() {
                return Err(format!("`{model}` has no reasoning level to lower (`+l`) via {via}"));
            }
            Ok(None)
        }
        None if r.effort.is_none() && r.budget.is_none() => {
            let Some(d) = route.default_effort() else { return Ok(None) };
            let note = match named.first() {
                Some(top) if **top != d => Some(not_highest(&d, top)),
                None if d == "none" && route.budget_range().is_some() => {
                    Some("reasoning is off by default; add +h to think with the largest budget".to_string())
                }
                _ => None,
            };
            r.effort = Some(d);
            Ok(note)
        }
        None => Ok(None),
    }
}

/// The note for a default reasoning level below the highest one.
pub fn not_highest(current: &str, top: &str) -> String {
    if current == "none" {
        format!("reasoning is off by default (the highest level is {top}); add +h for the highest")
    } else {
        format!("reasoning effort is {current} by default, not the highest ({top}); add +h for the highest")
    }
}

/// Image/video generation options a route takes (see [`RouteCaps::generation`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Generation {
    pub quality: bool,
    pub count: bool,
    pub background: bool,
    pub seconds: bool,
}

/// Check `r` against the vendor's capabilities and the route's. Returns the
/// first problem as a user-facing message.
pub fn validate(model: &str, r: &Requested, route: &dyn RouteCaps) -> Result<(), String> {
    let via = route.name();
    for &t in &r.tools {
        let route_ok = route.tool(t);
        match (official::has_tool(model, t), route_ok) {
            (_, true) => {}
            (Some(true), false) => {
                return Err(format!(
                    "{via} does not support {} (`+{}`) for `{model}` — the official API has it",
                    t.describe(),
                    t.letter()
                ));
            }
            _ => return Err(format!("`{model}` has no {} (`+{}`)", t.describe(), t.letter())),
        }
    }
    if let Some(e) = &r.effort {
        if e == "none" {
            let listed = route.efforts().is_some_and(|l| l.iter().any(|x| x == "none"));
            if !listed && !route.can_disable_reasoning() {
                return Err(format!("`{model}` cannot turn reasoning off (e=none) via {via}"));
            }
        } else {
            match route.efforts() {
                Some(levels) if levels.iter().any(|x| x == e) => {}
                Some(levels) => {
                    return Err(format!(
                        "effort `{e}` is not available for `{model}` via {via}; levels: {}",
                        levels.join(", ")
                    ));
                }
                None => return Err(format!("`{model}` has no reasoning effort levels via {via}")),
            }
        }
    }
    if let Some(l) = r.level {
        let control = route.efforts().is_some() || route.budget() || route.can_disable_reasoning();
        if !control && !route.fixed_reasoning() {
            let flag = if l == Level::Highest { "+h" } else { "+l" };
            return Err(format!("`{model}` has no reasoning level to set ({flag}) via {via}"));
        }
    }
    if let Some(n) = r.budget {
        if !route.budget() {
            let instead = if route.efforts().is_some() { " (use e= instead)" } else { "" };
            return Err(format!("`{model}` has no reasoning token budget via {via}{instead}"));
        }
        if n == 0 {
            let off = route.can_disable_reasoning() || route.efforts().is_some_and(|l| l.iter().any(|x| x == "none"));
            return Err(format!(
                "budget= must be at least 1{}",
                if off { " (e=none turns reasoning off)" } else { "" }
            ));
        }
    }
    if let Some(v) = &r.verbosity {
        match official::verbosity_levels(model) {
            None => return Err(format!("`{model}` has no verbosity setting")),
            Some(levels) if !levels.contains(&v.as_str()) => {
                return Err(format!("verbosity `{v}` is not available; levels: {}", levels.join(", ")));
            }
            Some(_) if !route.verbosity() => return Err(format!("{via} does not support verbosity for `{model}`")),
            Some(_) => {}
        }
    }
    if r.image_size.is_some() || r.aspect_ratio.is_some() {
        let Some((sizes, ratios)) = official::image_options(model) else {
            return Err(format!("`{model}` has no image size/aspect options"));
        };
        if let Some(s) = &r.image_size
            && !sizes.contains(&s.as_str())
        {
            return Err(format!("image size {s} is not available for `{model}`; sizes: {}", sizes.join(", ")));
        }
        if let Some(a) = &r.aspect_ratio
            && !ratios.contains(&a.as_str())
        {
            return Err(format!("aspect ratio {a} is not available for `{model}`; ratios: {}", ratios.join(", ")));
        }
        if !route.image_options() {
            return Err(format!("{via} does not support image size/aspect options for `{model}`"));
        }
    }
    let g = route.generation();
    for (asked, ok, what, opt) in [
        (r.quality.is_some(), g.quality, "an image quality", "q="),
        (r.count.is_some(), g.count, "a number of images", "n="),
        (r.background.is_some(), g.background, "a background", "bg="),
        (r.seconds.is_some(), g.seconds, "a video length", "sec="),
    ] {
        if asked && !ok {
            return Err(format!("`{model}` does not take {what} (`{opt}`) via {via}"));
        }
    }
    if let Some(t) = &r.tier
        && !matches!(t.as_str(), "default" | "auto")
    {
        let tiers = route.tiers();
        if !tiers.iter().any(|x| x == t) {
            return Err(if tiers.is_empty() {
                format!("`{model}` has no service tiers via {via}")
            } else {
                format!("tier `{t}` is not available for `{model}` via {via}; tiers: {}", tiers.join(", "))
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Caps {
        tools: Vec<Tool>,
        efforts: Option<Vec<String>>,
    }
    impl RouteCaps for Caps {
        fn name(&self) -> &str {
            "OpenRouter"
        }
        fn tool(&self, t: Tool) -> bool {
            self.tools.contains(&t)
        }
        fn efforts(&self) -> Option<Vec<String>> {
            self.efforts.clone()
        }
        fn can_disable_reasoning(&self) -> bool {
            false
        }
        fn budget(&self) -> bool {
            false
        }
        fn verbosity(&self) -> bool {
            true
        }
        fn image_options(&self) -> bool {
            true
        }
        fn tiers(&self) -> Vec<String> {
            vec!["flex".into()]
        }
    }

    fn kv(k: &str, v: &str) -> OptionToken {
        OptionToken::KeyValue(k.into(), v.into())
    }

    #[test]
    fn parses_the_small_vocabulary() {
        let r = parse(&[
            OptionToken::Flag("s".into()),
            kv("size", "4k"),
            kv("ar", "16:9"),
            kv("e", "LOW"),
            kv("tier", "flex"),
        ])
        .unwrap();
        assert_eq!(r.tools, vec![Tool::Search]);
        assert_eq!(r.image_size.as_deref(), Some("4K"));
        assert_eq!(r.aspect_ratio.as_deref(), Some("16:9"));
        assert_eq!(r.effort.as_deref(), Some("low"));
        assert!(parse(&[OptionToken::Flag("x".into())]).is_err());
        assert!(parse(&[kv("temperature", "1")]).is_err());
        assert!(parse(&[OptionToken::Json(serde_json::json!({"a": 1}))]).is_err());
        assert!(parse(&[kv("ar", "16:9"), kv("ar", "9:16")]).is_err(), "conflicting ratios");
        assert!(parse(&[kv("e", "low"), kv("budget", "100")]).is_err());
        // The one-letter size and ratio shortcuts are gone (owner, 2026-11-13).
        for f in ["4", "1", "2", "W", "H", "S", "w"] {
            assert!(parse(&[OptionToken::Flag(f.into())]).is_err(), "{f}");
        }
        assert!(parse(&[kv("size", "3K")]).unwrap_err().contains("1K, 2K or 4K"));
        // h / l: the highest / lowest reasoning level, not with e= or budget=.
        assert_eq!(parse(&[OptionToken::Flag("sh".into())]).unwrap().level, Some(Level::Highest));
        assert_eq!(parse(&[OptionToken::Flag("l".into())]).unwrap().level, Some(Level::Lowest));
        assert!(parse(&[OptionToken::Flag("hl".into())]).is_err());
        assert!(parse(&[OptionToken::Flag("h".into()), kv("e", "low")]).unwrap_err().contains("+h"));
        assert!(parse(&[OptionToken::Flag("l".into()), kv("budget", "10")]).is_err());
    }

    /// Defaults are the official ones; +h / +l pick the ends of the range.
    #[test]
    fn reasoning_levels_resolve() {
        struct R {
            efforts: Option<Vec<&'static str>>,
            default: Option<&'static str>,
            off: bool,
            budget: Option<(u64, u64)>,
            fixed: bool,
        }
        impl RouteCaps for R {
            fn name(&self) -> &str {
                "X"
            }
            fn tool(&self, _: Tool) -> bool {
                false
            }
            fn efforts(&self) -> Option<Vec<String>> {
                self.efforts.as_ref().map(|l| l.iter().map(|s| s.to_string()).collect())
            }
            fn can_disable_reasoning(&self) -> bool {
                self.off
            }
            fn budget(&self) -> bool {
                self.budget.is_some()
            }
            fn verbosity(&self) -> bool {
                false
            }
            fn image_options(&self) -> bool {
                false
            }
            fn tiers(&self) -> Vec<String> {
                vec![]
            }
            fn default_effort(&self) -> Option<String> {
                self.default.map(str::to_string)
            }
            fn fixed_reasoning(&self) -> bool {
                self.fixed
            }
            fn budget_range(&self) -> Option<(u64, u64)> {
                self.budget
            }
        }
        let levels = R {
            efforts: Some(vec!["max", "high", "medium", "low"]),
            default: Some("medium"),
            off: true,
            budget: None,
            fixed: false,
        };
        let go = |caps: &R, level: Option<Level>| {
            let mut r = Requested { level, ..Default::default() };
            let note = resolve_reasoning("m", &mut r, caps);
            (r, note)
        };
        let (r, note) = go(&levels, None);
        assert_eq!(r.effort.as_deref(), Some("medium"));
        assert_eq!(
            note.unwrap().unwrap(),
            "reasoning effort is medium by default, not the highest (max); add +h for the highest"
        );
        assert_eq!(go(&levels, Some(Level::Highest)).0.effort.as_deref(), Some("max"));
        assert_eq!(go(&levels, Some(Level::Lowest)).0.effort.as_deref(), Some("none"), "off where it can be");
        let no_off = R { off: false, ..levels };
        assert_eq!(go(&no_off, Some(Level::Lowest)).0.effort.as_deref(), Some("low"));
        // A default that is the highest: no note; e= given: nothing changes.
        let top = R { default: Some("max"), ..no_off };
        assert_eq!(go(&top, None).1.unwrap(), None);
        let mut asked = Requested { effort: Some("low".into()), ..Default::default() };
        assert_eq!(resolve_reasoning("m", &mut asked, &top).unwrap(), None);
        assert_eq!(asked.effort.as_deref(), Some("low"));
        // Off by default ("none" listed as a level).
        let off_default = R {
            efforts: Some(vec!["xhigh", "high", "medium", "low", "none"]),
            default: Some("none"),
            off: false,
            budget: None,
            fixed: false,
        };
        assert!(
            go(&off_default, None)
                .1
                .unwrap()
                .unwrap()
                .starts_with("reasoning is off by default (the highest level is xhigh)")
        );
        // Budget-only models: +h / +l are the largest / smallest budget; off by default.
        let budget =
            R { efforts: None, default: Some("none"), off: false, budget: Some((64_000, 1_024)), fixed: false };
        assert_eq!(go(&budget, Some(Level::Highest)).0.budget, Some(64_000));
        assert_eq!(go(&budget, Some(Level::Lowest)).0.budget, Some(1_024));
        assert!(go(&budget, None).1.unwrap().unwrap().contains("largest budget"));
        // Fixed reasoning: accepted, nothing changes; no reasoning at all: an error.
        let fixed = R { efforts: None, default: None, off: false, budget: None, fixed: true };
        let (r, note) = go(&fixed, Some(Level::Highest));
        assert!(r.effort.is_none() && r.budget.is_none() && note.unwrap().is_none());
        let none = R { fixed: false, ..fixed };
        assert!(go(&none, Some(Level::Highest)).1.unwrap_err().contains("+h"));
        assert!(go(&none, Some(Level::Lowest)).1.unwrap_err().contains("+l"));
    }

    #[test]
    fn official_vs_route_errors() {
        let caps = Caps { tools: vec![Tool::Search], efforts: Some(vec!["high".into(), "low".into()]) };
        let code = Requested { tools: vec![Tool::Code], ..Default::default() };
        let e = validate("google/gemini-4-carbon", &code, &caps).unwrap_err();
        assert!(e.starts_with("OpenRouter does not support code execution"), "{e}");
        let fetch = Requested { tools: vec![Tool::Fetch], ..Default::default() };
        let e = validate("openai/gpt-6.1-sol", &fetch, &caps).unwrap_err();
        assert!(e.contains("has no URL fetching"), "{e}");
        let eff = Requested { effort: Some("max".into()), ..Default::default() };
        assert!(validate("openai/gpt-6.1-sol", &eff, &caps).unwrap_err().contains("levels: high, low"));
        let none = Requested { effort: Some("none".into()), ..Default::default() };
        assert!(validate("openai/gpt-6.1-sol", &none, &caps).is_err());
        let img = Requested { image_size: Some("4K".into()), aspect_ratio: Some("21:9".into()), ..Default::default() };
        assert!(validate("google/gemini-nano-banana-2.1", &img, &caps).is_ok());
        let img = Requested { aspect_ratio: Some("7:3".into()), ..Default::default() };
        assert!(validate("google/gemini-nano-banana-2.1", &img, &caps).is_err());
        let v = Requested { verbosity: Some("max".into()), ..Default::default() };
        assert!(validate("openai/gpt-6.1-sol", &v, &caps).is_err());
        let t = Requested { tier: Some("priority".into()), ..Default::default() };
        assert!(validate("openai/gpt-6.1-sol", &t, &caps).unwrap_err().contains("tiers: flex"));
    }
}
