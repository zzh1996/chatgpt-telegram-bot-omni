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
}

impl Tool {
    pub fn letter(self) -> char {
        match self {
            Tool::Search => 's',
            Tool::Code => 'c',
            Tool::Fetch => 'u',
            Tool::Maps => 'm',
        }
    }
    pub fn describe(self) -> &'static str {
        match self {
            Tool::Search => "web search",
            Tool::Code => "code execution",
            Tool::Fetch => "URL fetching",
            Tool::Maps => "Google Maps",
        }
    }
}

/// Parsed options (what was asked for, not yet validated).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Requested {
    pub tools: Vec<Tool>,
    /// Reasoning effort level; `none` disables reasoning.
    pub effort: Option<String>,
    /// Reasoning token budget.
    pub budget: Option<u64>,
    pub verbosity: Option<String>,
    /// `1K` / `2K` / `4K`.
    pub image_size: Option<String>,
    pub aspect_ratio: Option<String>,
    /// Provider slugs (route-specific meaning).
    pub providers: Option<Vec<String>>,
    pub tier: Option<String>,
}

/// Help for the option vocabulary.
pub const HELP: &str = "\
s                 the provider's own web search
c                 the provider's own code execution
u                 let the model read URLs from the prompt (provider's own tool)
m                 Google Maps grounding (Gemini API); a Telegram location in the conversation
                  (sent, or replied to) is the user's position
e=<level>         reasoning effort, from the levels the model supports; e=none turns reasoning off
                  (Volcengine Ark: e=minimal turns thinking off too)
budget=<n>        reasoning token budget (models with budget-based thinking)
v=<level>         verbosity (low|medium|high; OpenAI)
1 2 4             image size 1K / 2K / 4K (image models)
S W H w h         image aspect ratio 1:1 / 16:9 / 9:16 / 4:3 / 3:4;  ar=<w:h> for any ratio the model supports
p=<slug>          another official provider of the model (e.g. p=google-vertex); p=a|b: any of these
                  (OpenRouter only)
tier=<tier>       service tier (flex; higher tiers only for users on the priority list;
                  default / auto change nothing)
Letters can be combined: gi+4W$ = 4K, 16:9.
Anything else is an error: there are no free-form request fields.";

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
                        's' | 'c' | 'u' | 'm' => {
                            let tool = match c {
                                's' => Tool::Search,
                                'c' => Tool::Code,
                                'u' => Tool::Fetch,
                                _ => Tool::Maps,
                            };
                            if !r.tools.contains(&tool) {
                                r.tools.push(tool);
                            }
                        }
                        '1' => set(&mut r.image_size, "1K".to_string(), "image size")?,
                        '2' => set(&mut r.image_size, "2K".to_string(), "image size")?,
                        '4' => set(&mut r.image_size, "4K".to_string(), "image size")?,
                        'S' => set(&mut r.aspect_ratio, "1:1".to_string(), "aspect ratio")?,
                        'W' => set(&mut r.aspect_ratio, "16:9".to_string(), "aspect ratio")?,
                        'H' => set(&mut r.aspect_ratio, "9:16".to_string(), "aspect ratio")?,
                        'w' => set(&mut r.aspect_ratio, "4:3".to_string(), "aspect ratio")?,
                        'h' => set(&mut r.aspect_ratio, "3:4".to_string(), "aspect ratio")?,
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
                other => return Err(format!("unknown option `{other}=`; see /help options")),
            },
        }
    }
    if r.effort.is_some() && r.budget.is_some() {
        return Err("use either e= or budget=, not both".into());
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
            ("openai", Tool::Search | Tool::Code) => true,
            ("openai", Tool::Fetch) => false,
            ("anthropic", _) => true, // web_search, code_execution, web_fetch
            ("google", _) => true,    // google_search, code_execution, url_context
            ("x-ai", Tool::Search | Tool::Code) => true,
            ("x-ai", Tool::Fetch) => false,
            ("deepseek" | "z-ai" | "moonshotai" | "bytedance-seed" | "bytedance", Tool::Search) => true,
            ("qwen", Tool::Search | Tool::Code) => true,
            ("deepseek" | "z-ai" | "moonshotai" | "bytedance-seed" | "bytedance" | "qwen", _) => false,
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
    if let Some(n) = r.budget {
        if !route.budget() {
            return Err(format!("`{model}` has no reasoning token budget via {via} (use e= instead)"));
        }
        if n == 0 {
            return Err("budget= must be at least 1 (use e=none to turn reasoning off)".into());
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
        let r = parse(&[OptionToken::Flag("s4W".into()), kv("e", "LOW"), kv("tier", "flex")]).unwrap();
        assert_eq!(r.tools, vec![Tool::Search]);
        assert_eq!(r.image_size.as_deref(), Some("4K"));
        assert_eq!(r.aspect_ratio.as_deref(), Some("16:9"));
        assert_eq!(r.effort.as_deref(), Some("low"));
        assert!(parse(&[OptionToken::Flag("x".into())]).is_err());
        assert!(parse(&[kv("temperature", "1")]).is_err());
        assert!(parse(&[OptionToken::Json(serde_json::json!({"a": 1}))]).is_err());
        assert!(parse(&[OptionToken::Flag("WH".into())]).is_err(), "conflicting ratios");
        assert!(parse(&[kv("e", "low"), kv("budget", "100")]).is_err());
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
