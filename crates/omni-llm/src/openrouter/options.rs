//! OpenRouter as a route: which of the vendor's capabilities it can deliver
//! ([`OrCaps`]), which of its API skins is closest to each vendor's own API
//! ([`dialect_for`]), and the request plan ([`Plan`]): the bot's defaults (the
//! highest the model officially offers) combined with the user's validated
//! options. Dialect modules turn a plan into a request body.

use serde_json::{Value, json};

use super::catalog::{EndpointInfo, ModelInfo};
use crate::request::{self, Requested, RouteCaps, Tool};

#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// Where a setting came from: `default`, `option`, `budget` (the bot's max_tokens).
    pub source: String,
    pub patch: Value,
}

#[derive(Debug, Clone, Default)]
pub struct FidelityConfig {
    /// author (e.g. `openai`) -> first-party provider slug (e.g. `openai`).
    pub first_party: Vec<(String, String)>,
    pub pin_first_party: bool,
    pub require_parameters: bool,
    pub disable_context_compression: bool,
    /// Upper bound for the automatic `max_tokens` (the catalog reports up to ~1M for
    /// some models; OpenRouter reserves credit for `max_tokens`, so an unbounded value
    /// can cause 402s under concurrency). `None` = no cap.
    pub max_tokens_cap: Option<u64>,
    /// Model patterns with Gemini-style "long decode" (output beyond the
    /// per-request limit via continuation tokens) and their total budget.
    pub long_decode: Vec<String>,
    pub long_decode_budget: u64,
}

impl FidelityConfig {
    pub fn standard() -> Self {
        Self {
            first_party: default_first_party(),
            pin_first_party: true,
            require_parameters: true,
            disable_context_compression: true,
            max_tokens_cap: Some(393_216),
            long_decode: vec!["google/gemini-4*".into()],
            long_decode_budget: 393_216,
        }
    }

    pub fn is_long_decode(&self, model: &str) -> bool {
        let base = model.split(':').next().unwrap_or(model);
        self.long_decode.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => base.starts_with(prefix),
            None => base == p,
        })
    }

    pub fn first_party_for(&self, model: &str) -> Option<&str> {
        let author = model.split('/').next()?;
        self.first_party.iter().find(|(a, _)| a == author).map(|(_, s)| s.as_str())
    }
}

pub fn default_first_party() -> Vec<(String, String)> {
    [
        ("openai", "openai"),
        ("anthropic", "anthropic"),
        ("google", "google-ai-studio"),
        ("x-ai", "xai"),
        ("deepseek", "deepseek"),
        ("z-ai", "z-ai"),
        ("moonshotai", "moonshotai"),
        ("qwen", "alibaba"),
        ("bytedance-seed", "seed"),
    ]
    .into_iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect()
}

/// OpenRouter API skin used for a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// `/chat/completions` — the vendors' OpenAI-compatible API (Gemini, DeepSeek, GLM, Kimi, Qwen, …).
    Chat,
    /// `/responses` — OpenAI's own API (also xAI's recommended one).
    Responses,
    /// `/messages` — Anthropic's own API.
    Messages,
}

/// The skin closest to the vendor's own API with the most features.
pub fn dialect_for(model: &str) -> Dialect {
    match model.split('/').next().unwrap_or("") {
        "openai" | "x-ai" => Dialect::Responses,
        "anthropic" => Dialect::Messages,
        _ => Dialect::Chat,
    }
}

const TIER_SEGMENTS: &[&str] = &["flex", "priority", "fast", "ultrafast", "scale"];

/// What OpenRouter can deliver for a model through the pinned endpoint(s).
pub struct OrCaps<'a> {
    pub model: &'a str,
    pub info: Option<&'a ModelInfo>,
    /// All endpoints of the model (empty if unknown).
    pub endpoints: &'a [EndpointInfo],
    /// Base provider slugs the request is pinned to (empty = no pin).
    pub pinned: Vec<String>,
    pub dialect: Dialect,
}

impl OrCaps<'_> {
    fn pinned_endpoints(&self) -> Vec<&EndpointInfo> {
        self.endpoints
            .iter()
            .filter(|e| {
                let tag = e.tag.as_deref().unwrap_or("");
                // the default tier of each pinned provider (any region), not tier endpoints
                let is_tier = tag.split('/').skip(1).any(|s| TIER_SEGMENTS.contains(&s));
                !is_tier && e.base_slug().is_some_and(|b| self.pinned.is_empty() || self.pinned.iter().any(|p| p == b))
            })
            .collect()
    }

    fn all_pinned_have(&self, native_tool: &str) -> bool {
        let eps = self.pinned_endpoints();
        !eps.is_empty() && eps.iter().all(|e| e.has_native_tool(native_tool))
    }
}

impl RouteCaps for OrCaps<'_> {
    fn name(&self) -> &str {
        "OpenRouter"
    }

    fn tool(&self, tool: Tool) -> bool {
        let author = self.model.split('/').next().unwrap_or("");
        match tool {
            // Only the provider's own search (the endpoint maps it to a native tool).
            Tool::Search => self.all_pinned_have("openrouter:web_search"),
            // OpenAI's hosted code_interpreter is passed through on the Responses skin.
            Tool::Code => author == "openai" && self.dialect == Dialect::Responses,
            Tool::Fetch => self.all_pinned_have("openrouter:web_fetch"),
            // Gemini's Google Maps grounding is not reachable through OpenRouter.
            Tool::Maps => false,
            // OpenRouter replaces OpenAI's image_generation tool with its own (D28).
            Tool::Image => false,
        }
    }

    fn efforts(&self) -> Option<Vec<String>> {
        self.info?.reasoning.as_ref()?.supported_efforts.clone()
    }

    fn can_disable_reasoning(&self) -> bool {
        self.info.and_then(|i| i.reasoning.as_ref()).is_some_and(|r| r.mandatory == Some(false))
    }

    fn budget(&self) -> bool {
        self.info.and_then(|i| i.reasoning.as_ref()).is_some_and(|r| r.supports_max_tokens == Some(true))
    }

    /// The level the vendor's own API gets by default for the same model
    /// ([`direct_default`]), when OpenRouter offers it (owner, 2026-11-14: the
    /// catalog's `default_effort` is a preselect for client UIs, not the
    /// vendor's default). Not offered: nothing is sent and the provider decides.
    fn default_effort(&self) -> Option<String> {
        let d = direct_default(self.model)?;
        self.efforts()?.contains(&d).then_some(d)
    }

    /// Mandatory reasoning without levels or a budget: nothing to set.
    fn fixed_reasoning(&self) -> bool {
        self.info.and_then(|i| i.reasoning.as_ref()).is_some_and(|r| {
            r.mandatory == Some(true) && r.supported_efforts.is_none() && r.supports_max_tokens != Some(true)
        })
    }

    fn verbosity(&self) -> bool {
        self.info.is_some_and(|i| i.supports("verbosity"))
    }

    fn image_options(&self) -> bool {
        self.info.is_some_and(|i| i.outputs("image")) && self.dialect == Dialect::Chat
    }

    fn tiers(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for e in self.endpoints {
            let Some(tag) = e.tag.as_deref() else { continue };
            let base = tag.split('/').next().unwrap_or(tag);
            if !(self.pinned.is_empty() || self.pinned.iter().any(|p| p == base)) {
                continue;
            }
            for seg in tag.split('/').skip(1) {
                if TIER_SEGMENTS.contains(&seg) && !out.iter().any(|x| x == seg) {
                    out.push(seg.to_string());
                }
            }
        }
        out
    }
}

/// The reasoning level the bot uses by default on the vendor's own API for the
/// same model (its model tables), so that an OpenRouter twin answers like the
/// direct prefix. `None`: no such model, or no named default.
pub fn direct_default(model: &str) -> Option<String> {
    let (author, id) = model.split_once('/')?;
    let id = id.split(':').next().unwrap_or(id);
    match author {
        "openai" => {
            let id = if id == "gpt-chat-latest" { "chat-latest" } else { id };
            // OpenRouter's `-pro` ids of GPT-6 are the base model in pro mode.
            let spec = crate::openai::model_spec(id)
                .or_else(|| id.strip_suffix("-pro").and_then(crate::openai::model_spec))?;
            spec.effort.map(str::to_string)
        }
        "anthropic" => crate::anthropic::models::model_spec(&id.replace('.', "-")).map(|s| s.effort.to_string()),
        "google" => crate::google::models::known(id)
            .then(|| crate::google::models::spec(id).default_level)
            .flatten()
            .map(str::to_string),
        "x-ai" => crate::xai::model_spec(id).and_then(|s| s.effort).map(str::to_string),
        "deepseek" => id.starts_with("deepseek-v4").then(|| crate::deepseek::DEFAULT_EFFORT.to_string()),
        "z-ai" => crate::zhipu::model_spec(id).and_then(|s| s.thinking.efforts().first()).map(|e| e.to_string()),
        "moonshotai" => crate::moonshot::model_spec(id).map(|_| "max".to_string()),
        // The direct API's table (Qwen4 Max `high`, Qwen3.8 `xhigh`); OpenRouter-only Qwen3.8 ids too.
        "qwen" => crate::alibaba::model_spec(id)
            .and_then(|s| s.default_effort)
            .or_else(|| id.starts_with("qwen3.8").then_some("xhigh"))
            .map(str::to_string),
        _ => None,
    }
}

/// The direct API's table entry for an OpenAI model on OpenRouter.
fn openai_spec(model: &str) -> Option<&'static crate::openai::ModelSpec> {
    let id = model.strip_prefix("openai/")?;
    let id = if id == "gpt-chat-latest" { "chat-latest" } else { id };
    crate::openai::model_spec(id).or_else(|| id.strip_suffix("-pro").and_then(crate::openai::model_spec))
}

/// Whether the direct API takes `reasoning.context: "all_turns"` for this OpenAI model.
fn direct_all_turns(model: &str) -> bool {
    openai_spec(model).is_some_and(|s| s.all_turns)
}

/// Whether OpenAI drops the earlier answers' reasoning in this request: GPT-5.2,
/// 5.1 and 5 keep only the current turn's (they take no `all_turns`), on
/// OpenRouter as on the direct API.
pub fn keeps_current_turn_only(model: &str, plan: &Plan) -> bool {
    plan.context.is_none()
        && matches!(&plan.reasoning, Reasoning::Effort(e) if e != "none")
        && openai_spec(model).is_some_and(|s| s.efforts.is_some() && !s.all_turns)
}

/// The note for a direct-API default level OpenRouter cannot request (Nano
/// Banana 2.1's `medium`): nothing was sent, the provider applies its default.
pub fn unsent_default_note(model: &str, req: &Requested, caps: &OrCaps<'_>) -> Option<String> {
    if req.effort.is_some() || req.budget.is_some() || req.level.is_some() {
        return None;
    }
    let d = direct_default(model)?;
    let levels = caps.efforts()?;
    if levels.contains(&d) {
        return None;
    }
    let top = levels.iter().find(|l| *l != "none")?;
    (d != *top).then(|| request::not_highest(&d, top))
}

/// Reasoning part of a plan.
#[derive(Debug, Clone, PartialEq)]
pub enum Reasoning {
    /// Not a reasoning model (send nothing).
    Unset,
    /// Reasoning on with the model's own default depth (no effort levels exposed).
    On,
    Off,
    Effort(String),
    Budget(u64),
}

/// A validated request, independent of the API skin.
#[derive(Debug, Clone)]
pub struct Plan {
    pub reasoning: Reasoning,
    /// Detailed reasoning summaries (OpenAI).
    pub summary: Option<&'static str>,
    pub verbosity: Option<String>,
    pub tools: Vec<Tool>,
    /// The model outputs images (request image + text output).
    pub image_output: bool,
    pub image_size: Option<String>,
    pub aspect_ratio: Option<String>,
    /// Highest `image_url.detail` the vendor offers for image input (OpenAI only).
    pub image_detail: Option<&'static str>,
    /// `reasoning.context: "all_turns"` (OpenAI models that take it: earlier
    /// turns' reasoning stays in context, as on the direct API).
    pub context: Option<&'static str>,
    /// Notes for the reply (generation settings below the highest).
    pub notes: Vec<String>,
    pub max_tokens: Option<u64>,
    pub tier: Option<String>,
    /// `provider.only` (first-party or the user's choice), with tier endpoints added.
    pub providers: Vec<String>,
    /// Human-readable settings (footer).
    pub described: Vec<String>,
    /// Where each setting came from (/info).
    pub applied: Vec<Applied>,
}

/// Merge `over` onto `base` field by field (presets first, then the user's options).
pub fn overlay(base: &Requested, over: &Requested) -> Requested {
    let mut r = base.clone();
    for t in &over.tools {
        if !r.tools.contains(t) {
            r.tools.push(*t);
        }
    }
    macro_rules! take {
        ($f:ident) => {
            if over.$f.is_some() {
                r.$f = over.$f.clone();
            }
        };
    }
    take!(effort);
    take!(budget);
    take!(level);
    take!(verbosity);
    take!(image_size);
    take!(aspect_ratio);
    take!(providers);
    take!(tier);
    take!(quality);
    take!(count);
    take!(background);
    take!(seconds);
    take!(audio);
    take!(rewrite);
    // One reasoning setting wins: the later one (a preset's e= gives way to a typed +h).
    if over.effort.is_some() {
        r.budget = None;
        r.level = None;
    }
    if over.budget.is_some() {
        r.effort = None;
        r.level = None;
    }
    if over.level.is_some() {
        r.effort = None;
        r.budget = None;
    }
    r
}

/// Build the plan: defaults (the reasoning level settled by
/// `request::resolve_reasoning`, the highest image detail and output budget)
/// plus the validated options `req` (`user` = what the user typed, for attribution).
pub fn plan(
    model: &str,
    info: Option<&ModelInfo>,
    caps: &OrCaps<'_>,
    req: &Requested,
    user: &Requested,
    auto_max_tokens: Option<u64>,
) -> Result<Plan, String> {
    request::validate(model, req, caps)?;
    let mut described = Vec::new();
    let mut applied = Vec::new();
    let src = |by_user: bool| if by_user { "option" } else { "default" };

    // Reasoning: the level settled by `request::resolve_reasoning` (the catalog's default,
    // +h, +l, e=); none settled: OpenRouter's (and the vendor's) own default.
    let none_listed = caps.efforts().is_some_and(|l| l.iter().any(|x| x == "none"));
    let reasoning = match (&req.effort, req.budget) {
        (Some(e), _) if e == "none" && !none_listed => Reasoning::Off,
        (Some(e), _) => Reasoning::Effort(e.clone()),
        (None, Some(n)) => Reasoning::Budget(n),
        (None, None) => Reasoning::Unset,
    };
    let user_reasoning = user.effort.is_some() || user.budget.is_some() || user.level.is_some();
    let rdesc = match &reasoning {
        Reasoning::Unset => None,
        Reasoning::On => Some("reasoning on".to_string()),
        Reasoning::Off => Some("reasoning off".to_string()),
        Reasoning::Effort(e) => Some(format!("effort {e}")),
        Reasoning::Budget(n) => Some(format!("thinking budget {n}")),
    };
    if let Some(d) = rdesc {
        applied.push(Applied { source: src(user_reasoning).into(), patch: json!({"reasoning": d}) });
        described.push(d);
    }
    let author = model.split('/').next().unwrap_or("");
    let summary =
        (author == "openai" && matches!(&reasoning, Reasoning::Effort(e) if e != "none")).then_some("detailed");
    if let Some(s) = summary {
        applied.push(Applied { source: "default".into(), patch: json!({"reasoning summary": s}) });
    }
    if let Some(v) = &req.verbosity {
        described.push(format!("verbosity {v}"));
        applied.push(Applied { source: src(user.verbosity.is_some()).into(), patch: json!({"verbosity": v}) });
    }
    for t in &req.tools {
        described.push(t.describe().to_string());
        applied.push(Applied { source: src(user.tools.contains(t)).into(), patch: json!({"tool": t.describe()}) });
    }
    let image_output = info.is_some_and(|i| i.outputs("image"));
    let mut notes = Vec::new();
    // D79: Google's default image size (1K, not sent), or a larger one at the same price.
    let free_size = model
        .strip_prefix("google/")
        .filter(|_| image_output)
        .and_then(|m| crate::google::models::spec(m).image_out)
        .and_then(|o| o.free_size);
    let image_size = req.image_size.clone().or(free_size.map(str::to_string));
    if let Some(s) = &image_size {
        described.push(format!("image size {s}"));
        applied.push(Applied { source: src(user.image_size.is_some()).into(), patch: json!({"image size": s}) });
    }
    if let Some((sizes, _)) =
        request::official::image_options(model).filter(|_| image_output && user.image_size.is_none())
    {
        let size = image_size.as_deref().unwrap_or("1K");
        if let Some(top) = sizes.last().filter(|t| **t != size) {
            notes.extend(request::raise_note(&[request::Raise::new("image size", size, *top, format!("+size={top}"))]));
        }
    }
    let context =
        (author == "openai" && matches!(&reasoning, Reasoning::Effort(e) if e != "none") && direct_all_turns(model))
            .then_some("all_turns");
    if let Some(c) = context {
        applied.push(Applied { source: "default".into(), patch: json!({"reasoning context": c}) });
    }
    if let Some(a) = &req.aspect_ratio {
        described.push(format!("aspect ratio {a}"));
        applied.push(Applied { source: src(user.aspect_ratio.is_some()).into(), patch: json!({"aspect ratio": a}) });
    }

    // Providers: the user's choice or the vendor's own endpoint; tier endpoints added.
    let mut providers: Vec<String> = caps.pinned.clone();
    if let Some(p) = &req.providers {
        described.push(format!("provider {}", p.join("|")));
        applied.push(Applied { source: "option".into(), patch: json!({"provider": p}) });
        providers = p.clone();
    }
    let tier = req.tier.clone().filter(|t| !matches!(t.as_str(), "default" | "auto"));
    if let Some(t) = &tier {
        described.push(format!("tier {t}"));
        applied.push(Applied { source: "option".into(), patch: json!({"service tier": t}) });
        let tiered: Vec<String> = providers.iter().filter(|p| !p.contains('/')).map(|p| format!("{p}/{t}")).collect();
        providers.extend(tiered);
    }
    if let Some(n) = auto_max_tokens {
        applied.push(Applied { source: "budget".into(), patch: json!({"max output tokens": n}) });
    }
    Ok(Plan {
        reasoning,
        summary,
        verbosity: req.verbosity.clone(),
        tools: req.tools.clone(),
        image_output,
        image_size,
        aspect_ratio: req.aspect_ratio.clone(),
        image_detail: request::official::max_image_detail(model),
        context,
        notes,
        max_tokens: auto_max_tokens,
        tier,
        providers,
        described,
        applied,
    })
}

/// OpenRouter routing fields shared by all skins.
pub fn routing(plan: &Plan, fidelity: &FidelityConfig) -> Value {
    let mut provider = json!({"allow_fallbacks": false});
    if fidelity.require_parameters {
        provider["require_parameters"] = json!(true);
    }
    if !plan.providers.is_empty() {
        provider["only"] = json!(plan.providers);
    }
    let mut v = json!({"provider": provider});
    if fidelity.disable_context_compression {
        v["plugins"] = json!([{"id": "context-compression", "enabled": false}]);
    }
    if let Some(t) = &plan.tier {
        v["service_tier"] = json!(t);
    }
    v
}

/// The provider's own web search (never a fallback engine).
pub fn search_tool() -> Value {
    json!({"type": "openrouter:web_search", "parameters": {"engine": "native"}})
}

/// The provider's own URL fetching (Anthropic `web_fetch`).
pub fn fetch_tool() -> Value {
    json!({"type": "openrouter:web_fetch", "parameters": {"engine": "native"}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_core::trigger::OptionToken;

    fn info(efforts: &[&str]) -> ModelInfo {
        serde_json::from_value(json!({
            "id": "openai/gpt-6.1-sol",
            "architecture": {"input_modalities": ["text", "image"], "output_modalities": ["text"]},
            "supported_parameters": ["reasoning", "max_tokens", "verbosity"],
            "reasoning": {"supported_efforts": efforts, "default_effort": "medium", "mandatory": false},
            "top_provider": {"max_completion_tokens": 128000}
        }))
        .unwrap()
    }

    fn ep(tag: &str, tools: Value) -> EndpointInfo {
        serde_json::from_value(json!({"tag": tag, "native_tools": tools})).unwrap()
    }

    /// The reasoning level `resolve_reasoning` settles, then the plan (as `prepare` does).
    fn resolved(caps: &OrCaps<'_>, mut req: Requested) -> (Requested, Option<String>) {
        let note = request::resolve_reasoning(caps.model, &mut req, caps).unwrap();
        (req, note)
    }

    #[test]
    fn gpt_5_2_keeps_only_the_current_turns_reasoning() {
        let i = info(&["xhigh", "high", "medium", "low", "none"]);
        let eps = vec![ep("openai", json!({}))];
        let caps = OrCaps {
            model: "openai/gpt-5.2",
            info: Some(&i),
            endpoints: &eps,
            pinned: vec!["openai".into()],
            dialect: Dialect::Responses,
        };
        let none = Requested::default();
        let (req, _) = resolved(&caps, none.clone());
        let p = plan("openai/gpt-5.2", Some(&i), &caps, &req, &none, None).unwrap();
        assert_eq!(p.context, None, "5.2 takes no all_turns");
        assert!(keeps_current_turn_only("openai/gpt-5.2", &p));
        // Reasoning off: nothing to drop.
        let l = request::parse(&[OptionToken::Flag("l".into())]).unwrap();
        let (req, _) = resolved(&caps, l.clone());
        let p = plan("openai/gpt-5.2", Some(&i), &caps, &req, &l, None).unwrap();
        assert!(!keeps_current_turn_only("openai/gpt-5.2", &p));
    }

    #[test]
    fn defaults_are_the_direct_apis_ones() {
        let i = info(&["max", "high", "medium", "low", "none"]);
        let eps =
            vec![ep("openai", json!({"openrouter:web_search": {"type": "web_search"}})), ep("openai/flex", json!({}))];
        let caps = OrCaps {
            model: "openai/gpt-6.1-sol",
            info: Some(&i),
            endpoints: &eps,
            pinned: vec!["openai".into()],
            dialect: Dialect::Responses,
        };
        let none = Requested::default();
        let (req, note) = resolved(&caps, none.clone());
        let p = plan("openai/gpt-6.1-sol", Some(&i), &caps, &req, &none, Some(1000)).unwrap();
        assert_eq!(p.reasoning, Reasoning::Effort("medium".into()), "the direct API's default");
        // Earlier turns' reasoning kept, as on the direct API.
        assert_eq!(p.context, Some("all_turns"));
        assert!(!keeps_current_turn_only("openai/gpt-6.1-sol", &p));
        assert_eq!(
            note.as_deref(),
            Some("reasoning effort is medium by default, not the highest (max); add +h for the highest")
        );
        assert_eq!(p.summary, Some("detailed"));
        assert_eq!(p.image_detail, Some("high"));
        assert_eq!(p.described, vec!["effort medium"]);
        // +h: the top level; +l: off (none is listed).
        let h = request::parse(&[OptionToken::Flag("h".into())]).unwrap();
        let (req, note) = resolved(&caps, h.clone());
        assert_eq!(
            plan("openai/gpt-6.1-sol", Some(&i), &caps, &req, &h, None).unwrap().reasoning,
            Reasoning::Effort("max".into())
        );
        assert!(note.is_none());
        let l = request::parse(&[OptionToken::Flag("l".into())]).unwrap();
        let (req, _) = resolved(&caps, l.clone());
        assert_eq!(
            plan("openai/gpt-6.1-sol", Some(&i), &caps, &req, &l, None).unwrap().reasoning,
            Reasoning::Effort("none".into())
        );
        let req =
            request::parse(&[OptionToken::Flag("sc".into()), OptionToken::KeyValue("tier".into(), "flex".into())])
                .unwrap();
        let (r, _) = resolved(&caps, req.clone());
        let p = plan("openai/gpt-6.1-sol", Some(&i), &caps, &r, &req, None).unwrap();
        assert_eq!(p.providers, vec!["openai", "openai/flex"]);
        assert_eq!(p.described, vec!["effort medium", "web search", "code execution", "tier flex"]);
        // URL fetching: OpenAI has none officially.
        let req = request::parse(&[OptionToken::Flag("u".into())]).unwrap();
        assert!(plan("openai/gpt-6.1-sol", Some(&i), &caps, &req, &req, None).is_err());
        // A model the direct APIs do not have: nothing is sent (the vendor's own default),
        // whatever the catalog's `default_effort` (a preselect for client UIs).
        let bare: ModelInfo = serde_json::from_value(
            json!({"id": "x/y", "reasoning": {"supported_efforts": ["high", "low"], "default_effort": "low"}}),
        )
        .unwrap();
        let caps = OrCaps { model: "x/y", info: Some(&bare), endpoints: &[], pinned: vec![], dialect: Dialect::Chat };
        let (req, note) = resolved(&caps, Requested::default());
        assert!(req.effort.is_none() && note.is_none());
        assert_eq!(plan("x/y", Some(&bare), &caps, &req, &req, None).unwrap().reasoning, Reasoning::Unset);
    }

    #[test]
    fn twins_default_like_the_direct_apis() {
        let d = |m: &str| direct_default(m);
        assert_eq!(d("anthropic/claude-opus-5.5").as_deref(), Some("medium"), "the catalog says high");
        assert_eq!(d("anthropic/claude-sonnet-4.6").as_deref(), Some("medium"));
        assert_eq!(d("anthropic/claude-fable-5.2").as_deref(), Some("high"));
        assert_eq!(d("google/gemini-3.1-pro-preview").as_deref(), Some("high"), "the catalog says medium");
        assert_eq!(d("google/gemini-4-hydrogen").as_deref(), Some("high"));
        assert_eq!(d("google/gemini-nano-banana-2.1").as_deref(), Some("medium"));
        assert_eq!(d("openai/gpt-5.4").as_deref(), Some("medium"));
        assert_eq!(d("openai/gpt-6-astra-pro").as_deref(), Some("medium"), "the base model's");
        assert_eq!(d("openai/gpt-chat-latest"), None);
        assert_eq!(d("x-ai/grok-4.3").as_deref(), Some("low"));
        assert_eq!(d("deepseek/deepseek-v4.1-flash-1017").as_deref(), Some("high"));
        assert_eq!(d("z-ai/glm-5.3").as_deref(), Some("max"));
        assert_eq!(d("moonshotai/kimi-k3").as_deref(), Some("max"));
        assert_eq!(d("qwen/qwen3.8-max-0902").as_deref(), Some("xhigh"));
        assert_eq!(d("qwen/qwen4-max").as_deref(), Some("high"));
        assert_eq!(d("qwen/qwen3.8-max-prime").as_deref(), Some("xhigh"));
        assert_eq!(d("qwen/qwen3.7-max"), None);
        // A default OpenRouter cannot request (Nano Banana 2.1's medium): nothing sent, but noted.
        let i: ModelInfo = serde_json::from_value(json!({
            "id": "google/gemini-nano-banana-2.1",
            "architecture": {"input_modalities": ["text", "image"], "output_modalities": ["image", "text"]},
            "reasoning": {"supported_efforts": ["high", "minimal", "none"], "default_effort": "minimal"}
        }))
        .unwrap();
        let caps = OrCaps {
            model: "google/gemini-nano-banana-2.1",
            info: Some(&i),
            endpoints: &[],
            pinned: vec![],
            dialect: Dialect::Chat,
        };
        let (req, note) = resolved(&caps, Requested::default());
        assert!(req.effort.is_none() && note.is_none());
        assert_eq!(
            unsent_default_note("google/gemini-nano-banana-2.1", &req, &caps).as_deref(),
            Some("reasoning effort is medium by default, not the highest (high); add +h for the highest")
        );
        // Its image size: Google's default (1K, not sent), with the note.
        let p = plan("google/gemini-nano-banana-2.1", Some(&i), &caps, &req, &req, None).unwrap();
        assert_eq!(p.image_size, None);
        assert_eq!(p.notes, ["image size is 1K by default, not the largest (4K); add +size=4K"]);
        assert_eq!(p.context, None, "not an OpenAI model");
    }

    #[test]
    fn overlay_user_over_presets() {
        let preset = Requested { image_size: Some("4K".into()), ..Default::default() };
        let user = Requested { image_size: Some("1K".into()), effort: Some("low".into()), ..Default::default() };
        let r = overlay(&preset, &user);
        assert_eq!(r.image_size.as_deref(), Some("1K"));
        assert_eq!(r.effort.as_deref(), Some("low"));
    }
}
