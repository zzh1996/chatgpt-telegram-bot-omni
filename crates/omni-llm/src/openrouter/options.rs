//! User-facing option vocabulary for OpenRouter requests (`+flag`, `+key=value`, `+{json}`)
//! and the "max capability" auto-presets.
//!
//! Resolution order (later wins), all as JSON merge patches on the request body:
//! 1. fidelity defaults (first-party provider pin, no fallbacks, no silent
//!    param dropping, context compression off) — disable with `+p=any`;
//! 2. auto-max presets derived from the live model catalog (highest reasoning
//!    effort, max output tokens, detailed summaries, image output) — disable with `+pure`;
//! 3. config presets matching the model;
//! 4. user options in the order typed;
//! 5. user JSON patches.
//!
//! Every step is recorded in [`Resolved::applied`] so the bot can show exactly
//! what was sent and why.

use omni_core::json::merge_patch;
use omni_core::trigger::OptionToken;
use serde_json::{Value, json};

use super::catalog::ModelInfo;

#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// Where the patch came from: `fidelity`, `auto-max`, `preset:<pattern>`, `option:<raw>`, `json`.
    pub source: String,
    pub patch: Value,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Resolved {
    pub body: Value,
    pub applied: Vec<Applied>,
    /// Display-only flags (not request params).
    pub show_debug: bool,
    /// Warnings to show the user (e.g. unsupported effort for this model).
    pub warnings: Vec<String>,
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
}

impl FidelityConfig {
    pub fn standard() -> Self {
        Self {
            first_party: default_first_party(),
            pin_first_party: true,
            require_parameters: true,
            disable_context_compression: true,
            max_tokens_cap: Some(393_216),
        }
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
        ("mistralai", "mistral"),
        ("meta-llama", "meta"),
        ("minimax", "minimax"),
        ("xiaomi", "xiaomi"),
    ]
    .into_iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect()
}

/// Help text for the option vocabulary (shown by /help options).
pub const OPTIONS_HELP: &str = "\
s | search | s=<engine>     web search via the provider's native tool (engine: native|auto|exa|parallel|firecrawl|perplexity)
e=<effort> | effort=<e>     reasoning effort: none|minimal|low|medium|high|xhigh|max
l                           shorthand for e=low (old bot flag)
nothink                     disable reasoning (effort none)
budget=<n>                  reasoning token budget (reasoning.max_tokens)
summary=<s>                 reasoning summary: auto|concise|detailed
max=<n>                     max output tokens
t=<x> | temp=<x>            temperature;  top_p=<x>  top_k=<n>  seed=<n>
v=<x> | verbosity=<x>       verbosity: low|medium|high|xhigh|max
tier=<x>                    service tier: auto|default|flex|priority|...
p=<slug>[|<slug>...]        pin provider(s) (p=any: let OpenRouter route, fallbacks allowed)
img | text                  output modalities image+text / text only
ar=<w:h> | 1k | 2k | 4k     image aspect ratio / size (S W H w h = 1:1 16:9 9:16 4:3 3:4)
pure                        no automatic max-capability presets
debug                       show the exact upstream request echoed by OpenRouter
{json}                      JSON merge patch applied to the request body last";

/// Resolve options into a request body patch on top of `base`.
pub fn resolve(
    base: Value,
    model: &str,
    info: Option<&ModelInfo>,
    fidelity: &FidelityConfig,
    config_presets: &[(String, Value)],
    options: &[OptionToken],
) -> Result<Resolved, String> {
    let mut r = Resolved { body: base, ..Default::default() };
    let pure = options.iter().any(|o| matches!(o, OptionToken::Flag(f) if f == "pure"));
    let any_provider = options.iter().any(
        |o| matches!(o, OptionToken::KeyValue(k, v) if (k == "p" || k == "provider") && (v == "any" || v == "auto")),
    );

    // 1. fidelity defaults
    let mut fid = json!({});
    if fidelity.disable_context_compression {
        fid["plugins"] = json!([{"id": "context-compression", "enabled": false}]);
    }
    if !any_provider {
        let mut provider = json!({"allow_fallbacks": false});
        if fidelity.require_parameters {
            provider["require_parameters"] = json!(true);
        }
        if fidelity.pin_first_party
            && let Some(slug) = fidelity.first_party_for(model)
        {
            provider["only"] = json!([slug]);
        }
        fid["provider"] = provider;
    }
    apply(&mut r, "fidelity", fid);

    // 2. auto-max presets from the catalog
    if !pure && let Some(info) = info {
        let mut p = json!({});
        if let Some(max) = info.top_provider.max_completion_tokens {
            let max = fidelity.max_tokens_cap.map_or(max, |c| max.min(c));
            p["max_tokens"] = json!(max);
        }
        if info.supports("reasoning") {
            let mut reasoning = json!({});
            match info.max_effort() {
                Some(e) => reasoning["effort"] = json!(e),
                // No effort levels published (e.g. Qwen): just make sure reasoning is on.
                None => reasoning["enabled"] = json!(true),
            }
            if info.author() == "openai" {
                reasoning["summary"] = json!("detailed");
            }
            p["reasoning"] = reasoning;
        }
        if info.outputs("image") {
            let mods: Vec<&str> = if info.outputs("text") { vec!["image", "text"] } else { vec!["image"] };
            p["modalities"] = json!(mods);
        }
        if p.as_object().is_some_and(|o| !o.is_empty()) {
            apply(&mut r, "auto-max", p);
        }
    }

    // 3. config presets
    for (pattern, patch) in config_presets {
        apply(&mut r, &format!("preset:{pattern}"), patch.clone());
    }

    // 4. user options
    for o in options {
        match o {
            OptionToken::Json(_) => {}
            OptionToken::Flag(f) => {
                if f == "pure" {
                    continue;
                }
                if f == "debug" {
                    r.show_debug = true;
                    apply(&mut r, "option:debug", json!({"debug": {"echo_upstream_body": true}}));
                    continue;
                }
                let patch = flag_patch(f, info)?;
                apply(&mut r, &format!("option:{f}"), patch);
            }
            OptionToken::KeyValue(k, v) => {
                if (k == "p" || k == "provider") && (v == "any" || v == "auto") {
                    r.applied.push(Applied {
                        source: format!("option:{k}={v}"),
                        patch: json!({"provider": "(OpenRouter default routing)"}),
                    });
                    continue;
                }
                let patch = kv_patch(k, v, info)?;
                apply(&mut r, &format!("option:{k}={v}"), patch);
            }
        }
    }

    // 5. JSON patches
    for o in options {
        if let OptionToken::Json(v) = o {
            apply(&mut r, "json", v.clone());
        }
    }

    validate_effort(&mut r, info);
    Ok(r)
}

fn apply(r: &mut Resolved, source: &str, patch: Value) {
    merge_patch(&mut r.body, &patch);
    r.applied.push(Applied { source: source.to_string(), patch });
}

fn search_tool(engine: &str) -> Value {
    json!({"tools": [{"type": "openrouter:web_search", "parameters": {"engine": engine}}]})
}

fn flag_patch(f: &str, _info: Option<&ModelInfo>) -> Result<Value, String> {
    Ok(match f {
        "s" | "search" | "web" => search_tool("native"),
        "l" => json!({"reasoning": {"effort": "low", "enabled": null}}),
        "nothink" => json!({"reasoning": {"effort": "none", "enabled": null, "summary": null}}),
        "img" | "image" => json!({"modalities": ["image", "text"]}),
        "text" => json!({"modalities": ["text"]}),
        "1k" | "1K" | "1" => json!({"image_config": {"image_size": "1K"}}),
        "2k" | "2K" | "2" => json!({"image_config": {"image_size": "2K"}}),
        "4k" | "4K" | "4" => json!({"image_config": {"image_size": "4K"}}),
        "S" => json!({"image_config": {"aspect_ratio": "1:1"}}),
        "W" => json!({"image_config": {"aspect_ratio": "16:9"}}),
        "H" => json!({"image_config": {"aspect_ratio": "9:16"}}),
        "w" => json!({"image_config": {"aspect_ratio": "4:3"}}),
        "h" => json!({"image_config": {"aspect_ratio": "3:4"}}),
        "json" => json!({"response_format": {"type": "json_object"}}),
        other => {
            // Old-style concatenated single-letter flags, e.g. `+s4W`.
            if other.len() > 1 && other.chars().all(|c| "sl124SWHwh".contains(c)) {
                let mut acc = json!({});
                for c in other.chars() {
                    merge_patch(&mut acc, &flag_patch(&c.to_string(), _info)?);
                }
                return Ok(acc);
            }
            return Err(format!("unknown option `{other}`"));
        }
    })
}

fn num(v: &str) -> Result<Value, String> {
    if let Ok(i) = v.parse::<i64>() {
        return Ok(json!(i));
    }
    v.parse::<f64>().map(|f| json!(f)).map_err(|_| format!("`{v}` is not a number"))
}

fn kv_patch(k: &str, v: &str, _info: Option<&ModelInfo>) -> Result<Value, String> {
    Ok(match k {
        "s" | "search" | "web" => search_tool(v),
        "e" | "effort" | "reasoning" => json!({"reasoning": {"effort": v, "enabled": null}}),
        "budget" => json!({"reasoning": {"max_tokens": num(v)?, "effort": null}}),
        "summary" => json!({"reasoning": {"summary": v}}),
        "max" | "max_tokens" => json!({"max_tokens": num(v)?}),
        "t" | "temp" | "temperature" => json!({"temperature": num(v)?}),
        "top_p" => json!({"top_p": num(v)?}),
        "top_k" => json!({"top_k": num(v)?}),
        "seed" => json!({"seed": num(v)?}),
        "v" | "verbosity" => json!({"verbosity": v}),
        "tier" | "service_tier" => json!({"service_tier": v}),
        "p" | "provider" => {
            let slugs: Vec<&str> = v.split('|').map(str::trim).filter(|s| !s.is_empty()).collect();
            json!({"provider": {"only": slugs, "order": null}})
        }
        "ar" | "aspect" | "aspect_ratio" => json!({"image_config": {"aspect_ratio": v}}),
        "size" | "image_size" => json!({"image_config": {"image_size": v.to_ascii_uppercase()}}),
        other => return Err(format!("unknown option `{other}=`")),
    })
}

/// If the model publishes its effort list, warn (don't silently map) about unsupported values.
fn validate_effort(r: &mut Resolved, info: Option<&ModelInfo>) {
    let Some(effort) = r.body.pointer("/reasoning/effort").and_then(Value::as_str).map(str::to_string) else { return };
    let Some(info) = info else { return };
    let Some(ri) = &info.reasoning else { return };
    if let Some(list) = &ri.supported_efforts
        && effort != "none"
        && !list.iter().any(|e| e == &effort)
    {
        r.warnings.push(format!(
            "effort `{effort}` is not in this model's supported list [{}]; OpenRouter will map it to the nearest level",
            list.join(", ")
        ));
    }
    if effort == "none" && ri.mandatory == Some(true) {
        r.warnings.push("reasoning is mandatory for this model; `none` will be rejected".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openrouter::catalog::*;

    fn info() -> ModelInfo {
        serde_json::from_value(json!({
            "id": "anthropic/claude-opus-5.5",
            "architecture": {"input_modalities": ["text", "image"], "output_modalities": ["text"]},
            "supported_parameters": ["reasoning", "max_tokens"],
            "reasoning": {"supported_efforts": ["max", "high"], "mandatory": true},
            "top_provider": {"max_completion_tokens": 128000}
        }))
        .unwrap()
    }

    fn base() -> Value {
        json!({"model": "anthropic/claude-opus-5.5", "stream": true})
    }

    #[test]
    fn defaults_are_max_and_pinned() {
        let r =
            resolve(base(), "anthropic/claude-opus-5.5", Some(&info()), &FidelityConfig::standard(), &[], &[]).unwrap();
        assert_eq!(
            r.body["provider"],
            json!({"allow_fallbacks": false, "require_parameters": true, "only": ["anthropic"]})
        );
        assert_eq!(r.body["reasoning"], json!({"effort": "max"}));
        assert_eq!(r.body["max_tokens"], json!(128000));
        assert_eq!(r.body["plugins"][0]["id"], "context-compression");
    }

    #[test]
    fn user_options_override_and_pure() {
        let opts = vec![
            OptionToken::Flag("pure".into()),
            OptionToken::KeyValue("e".into(), "low".into()),
            OptionToken::Flag("s".into()),
            OptionToken::Json(json!({"max_tokens": 10})),
        ];
        let r = resolve(base(), "anthropic/claude-opus-5.5", Some(&info()), &FidelityConfig::standard(), &[], &opts)
            .unwrap();
        assert_eq!(r.body["reasoning"], json!({"effort": "low"}));
        assert_eq!(r.body["max_tokens"], json!(10));
        assert_eq!(r.body["tools"][0]["type"], "openrouter:web_search");
        assert!(r.warnings.iter().any(|w| w.contains("`low`")));
    }

    #[test]
    fn any_provider_unpins() {
        let opts = vec![OptionToken::KeyValue("p".into(), "any".into())];
        let r = resolve(base(), "anthropic/claude-opus-5.5", Some(&info()), &FidelityConfig::standard(), &[], &opts)
            .unwrap();
        assert!(r.body.get("provider").is_none());
    }

    #[test]
    fn old_style_flags() {
        let opts = vec![OptionToken::Flag("s4W".into())];
        let r = resolve(base(), "google/gemini-x", None, &FidelityConfig::standard(), &[], &opts).unwrap();
        assert_eq!(r.body["image_config"], json!({"image_size": "4K", "aspect_ratio": "16:9"}));
        assert!(
            resolve(base(), "x/y", None, &FidelityConfig::standard(), &[], &[OptionToken::Flag("zzz".into())]).is_err()
        );
    }
}
