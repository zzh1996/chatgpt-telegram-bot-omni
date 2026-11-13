//! Request building: options checked against what the Gemini API offers for
//! the model (the API is the route), defaults at the maximum the model offers
//! (D3), and the body with its `applied` record and `settings`.
//!
//! Defaults: the highest `thinkingLevel` (Gemini 3/4) or the largest
//! `thinkingBudget` (2.5) with `includeThoughts` (thought summaries); the
//! output budget (`maxOutputTokens`: the model's per-request maximum, on
//! Gemini 4 the long-decode budget); the highest per-part media resolution.
//! Not sent: safety settings (every adjustable filter is off by default on
//! Gemini 2.5 and later: the safety guide, "If the threshold is not set, the
//! default block threshold is Off"), sampling parameters, `store`.
//!
//! Tools: `+s` is Google Search (web and image search on the models that have
//! image search), `+c` code execution, `+u` URL context, `+m` Google Maps
//! (with the latest location as `toolConfig.retrievalConfig.latLng`). On the
//! Gemini 3 and 4 text models, `toolConfig.includeServerSideToolInvocations`
//! asks for the search, Maps and URL-context calls and results as parts of
//! the answer where they ran (preview; sent with those tools, and whenever
//! the conversation already holds such parts: Google's replay rule says the
//! flag goes with them). Their parts go back verbatim in later turns, where
//! they count as input tokens (Google Search excepted).
//!
//! Which model served a deprecated id that Google routes automatically
//! (`gemini-3.7-flash` → `gemini-3.8-flash`, `gemini-2.5-pro` → `gemini-3.8-flash`
//! from 2026-12-01, …) is read from the response's `modelVersion` by the bot
//! (the footer's served model), not predicted here.

use serde_json::{Map, Value, json};

use super::convert::{self, RES_HIGH, RES_ULTRA_HIGH};
use super::models::{self, Caps, Family, ModelSpec, Thinking};
use super::{DIALECT, NAME};
use crate::error::LlmError;
use crate::openrouter::options::{Applied, overlay};
use crate::provider::DirectConfig;
use crate::request::{self, Requested, Tool};
use crate::{BlobLoader, CallInput, Prepared};

/// `labels.safety_identifier`: Cloud label values are at most 63 characters of
/// lowercase letters, digits, `_` and `-` (the bot's user hash is 64 hex digits).
pub fn label_value(user: &str) -> Option<String> {
    let v: String = user
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-')
        .take(63)
        .collect();
    (!v.is_empty()).then_some(v)
}

fn err(m: impl Into<String>) -> LlmError {
    LlmError::request(m)
}

/// Checks `request::validate` cannot do for this API: thinking budget ranges,
/// Search together with Maps, image options per official id.
fn check_more(model: &str, spec: &ModelSpec, req: &Requested) -> Result<(), LlmError> {
    if let (Thinking::Budget { min, max, .. }, Some(n)) = (spec.thinking, req.budget)
        && (n < min || n > max)
    {
        return Err(err(format!("budget={n} is out of range for `{model}`: {}–{max} thinking tokens", min.max(1))));
    }
    if req.tools.contains(&Tool::Search) && req.tools.contains(&Tool::Maps) && !spec.search_with_maps {
        return Err(err(format!(
            "`{model}` cannot use web search and Google Maps together (`+sm`): the Gemini API combines them on \
             Gemini 3.5 Flash and later"
        )));
    }
    if req.image_size.is_some() || req.aspect_ratio.is_some() {
        let Some(img) = spec.image_out else {
            return Err(err(format!("`{model}` has no image size/aspect options")));
        };
        if let Some(s) = &req.image_size
            && !img.sizes.contains(&s.as_str())
        {
            return Err(err(if img.sizes.is_empty() {
                format!("`{model}` has no image size option (one output size)")
            } else {
                format!("image size {s} is not available for `{model}`; sizes: {}", img.sizes.join(", "))
            }));
        }
        if let Some(a) = &req.aspect_ratio
            && !img.ratios.contains(&a.as_str())
        {
            return Err(err(format!(
                "aspect ratio {a} is not available for `{model}`; ratios: {}",
                img.ratios.join(", ")
            )));
        }
    }
    Ok(())
}

/// The thinking config and its description.
fn thinking(spec: &ModelSpec, req: &Requested) -> Option<(Value, String)> {
    let level = |l: &str| (json!({"thinkingLevel": l, "includeThoughts": true}), format!("effort {l}"));
    let budget = |n: u64| (json!({"thinkingBudget": n, "includeThoughts": true}), format!("thinking budget {n}"));
    let off = || (json!({"thinkingBudget": 0}), "thinking off".to_string());
    match spec.thinking {
        Thinking::Levels(levels) => Some(level(req.effort.as_deref().unwrap_or(levels[0]))),
        Thinking::Budget { max, .. } => Some(match req.effort.as_deref() {
            Some("none") => off(),
            _ => budget(req.budget.unwrap_or(max)),
        }),
        Thinking::AlwaysOn => Some((json!({"includeThoughts": true}), "reasoning on".to_string())),
        Thinking::Unsupported => None,
        Thinking::Unknown => match (req.effort.as_deref(), req.budget) {
            (Some("none"), _) => Some(off()),
            (Some(e), _) => Some(level(e)),
            (None, Some(n)) => Some(budget(n)),
            (None, None) => None,
        },
    }
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub async fn prepare(
    cfg: &DirectConfig,
    input: &CallInput<'_>,
    presets: &[Requested],
    blobs: &dyn BlobLoader,
) -> Result<Prepared, LlmError> {
    let model = input.model;
    if model.is_empty() || !model.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_')) {
        return Err(err(format!("`{model}` is not a Gemini API model id")));
    }
    let user = request::parse(input.options).map_err(LlmError::request)?;
    let mut req = Requested::default();
    for p in presets {
        req = overlay(&req, p);
    }
    let req = overlay(&req, &user);
    if req.providers.is_some() {
        return Err(err(format!("`p=` (provider choice) does not apply to {NAME}")));
    }
    let spec = models::spec(model);
    // Image options are checked against this module's table (the shared official
    // table is keyed by OpenRouter ids).
    let mut basic = req.clone();
    basic.image_size = None;
    basic.aspect_ratio = None;
    request::validate(model, &basic, &Caps(&spec)).map_err(LlmError::request)?;
    check_more(model, &spec, &req)?;

    let conv = convert::convert(input.history, model, &spec, blobs).await?;
    let maps = req.tools.contains(&Tool::Maps);
    if conv.location.is_some() && !maps {
        return Err(err(if spec.tools.contains(&Tool::Maps) {
            "the conversation contains a location: the Gemini API takes one only as your position for Google Maps \
             grounding; add `+m` to the model or prefix to send it"
                .to_string()
        } else {
            format!(
                "the conversation contains a location, which the Gemini API takes only as the position for Google \
                 Maps grounding (`+m`), and `{model}` has no Google Maps grounding"
            )
        }));
    }

    let src = |by_user: bool| if by_user { "option" } else { "default" }.to_string();
    let mut applied = Vec::new();
    let mut settings = Vec::new();
    let mut warnings = conv.warnings.clone();
    let mut generation = Map::new();

    // Output budget: the model's maximum per request; on Gemini 4 the total of all
    // long-decode hops, within the 1M window shared by input and output. Gemini 3
    // and 2.5 have separate input and output limits.
    let prompt = crate::budget::estimate_prompt(input.history, input.known_context);
    let auto = if spec.unified {
        crate::budget::choose(Some(spec.context), Some(cfg.long_decode_budget), None, prompt)
    } else if spec.known {
        crate::budget::choose(None, Some(spec.max_output), cfg.max_tokens_cap, prompt)
    } else {
        // Unknown limit: the API's default is the model's maximum.
        None
    };
    let max_tokens = input.max_tokens_override.or(auto);
    if let Some(n) = max_tokens {
        generation.insert("maxOutputTokens".into(), json!(n));
        applied.push(Applied { source: "budget".into(), patch: json!({"generationConfig": {"maxOutputTokens": n}}) });
    }

    if let Some((config, desc)) = thinking(&spec, &req) {
        applied.push(Applied {
            source: src(user.effort.is_some() || user.budget.is_some()),
            patch: json!({"generationConfig": {"thinkingConfig": config}}),
        });
        generation.insert("thinkingConfig".into(), config);
        settings.push(desc);
    }

    if spec.per_part_media_resolution && (conv.media.image || conv.media.pdf || conv.media.video) {
        let mut m = Map::new();
        if conv.media.image {
            m.insert("images".into(), json!(RES_ULTRA_HIGH));
        }
        if conv.media.pdf {
            m.insert("PDFs".into(), json!(RES_HIGH));
        }
        if conv.media.video {
            m.insert("video".into(), json!(RES_HIGH));
        }
        applied.push(Applied { source: "default".into(), patch: json!({"mediaResolution (per part)": m}) });
    }

    let mut tools = Vec::new();
    let mut tool_config = Map::new();
    for t in &req.tools {
        let tool = match t {
            // Web and image search where the model has both (the default is web only).
            Tool::Search if spec.image_search => {
                json!({"googleSearch": {"searchTypes": {"webSearch": {}, "imageSearch": {}}}})
            }
            Tool::Search => json!({"googleSearch": {}}),
            Tool::Code => json!({"codeExecution": {}}),
            Tool::Fetch => json!({"urlContext": {}}),
            Tool::Maps => json!({"googleMaps": {}}),
            // Gemini has no image-generation tool (its image models draw by themselves).
            Tool::Image => {
                return Err(LlmError::request(format!("`{model}` has no {} (`+{}`)", t.describe(), t.letter())));
            }
        };
        applied.push(Applied { source: src(user.tools.contains(t)), patch: json!({"tools": [tool]}) });
        tools.push(tool);
        settings.push(match (t, conv.location) {
            (Tool::Maps, Some((lat, lng))) => format!("Google Maps near {lat:.4},{lng:.4}"),
            (Tool::Maps, None) => "Google Maps (no position sent)".to_string(),
            (Tool::Search, _) if spec.image_search => "web and image search".to_string(),
            _ => t.describe().to_string(),
        });
        if let (Tool::Maps, Some((lat, lng))) = (t, conv.location) {
            let rc = json!({"latLng": {"latitude": lat, "longitude": lng}});
            applied.push(Applied {
                source: src(user.tools.contains(t)),
                patch: json!({"toolConfig": {"retrievalConfig": rc}}),
            });
            tool_config.insert("retrievalConfig".into(), rc);
        }
    }
    // The built-in tools' calls and results as parts, where they ran; and with
    // a conversation that holds such parts already.
    let traced = req.tools.iter().any(|t| matches!(t, Tool::Search | Tool::Maps | Tool::Fetch));
    if spec.tool_invocations && (traced || convert::has_tool_invocations(&conv.contents)) {
        tool_config.insert("includeServerSideToolInvocations".into(), json!(true));
        applied.push(Applied {
            source: "default".into(),
            patch: json!({"toolConfig": {"includeServerSideToolInvocations": true}}),
        });
    }
    if maps && conv.media.multimodal() {
        warnings.push(
            "Google documents that Grounding with Google Maps does not support multimodal inputs beyond text; \
             this request has media"
                .to_string(),
        );
    }

    if spec.image_out.is_some() {
        // The image models' default, sent explicitly.
        let modalities = json!(["TEXT", "IMAGE"]);
        applied.push(Applied {
            source: "default".into(),
            patch: json!({"generationConfig": {"responseModalities": modalities}}),
        });
        generation.insert("responseModalities".into(), modalities);
        let mut ic = Map::new();
        if let Some(s) = &req.image_size {
            ic.insert("imageSize".into(), json!(s));
            settings.push(format!("image size {s}"));
            applied.push(Applied {
                source: src(user.image_size.is_some()),
                patch: json!({"generationConfig": {"imageConfig": {"imageSize": s}}}),
            });
        }
        if let Some(a) = &req.aspect_ratio {
            ic.insert("aspectRatio".into(), json!(a));
            settings.push(format!("aspect ratio {a}"));
            applied.push(Applied {
                source: src(user.aspect_ratio.is_some()),
                patch: json!({"generationConfig": {"imageConfig": {"aspectRatio": a}}}),
            });
        }
        if !ic.is_empty() {
            generation.insert("imageConfig".into(), Value::Object(ic));
        }
    }

    let tier = req.tier.clone().filter(|t| !matches!(t.as_str(), "default" | "auto"));
    if let Some(t) = &tier {
        settings.push(format!("tier {t}"));
        applied.push(Applied { source: src(user.tier.is_some()), patch: json!({"service_tier": t}) });
    }
    let label = input.user.as_deref().and_then(label_value);
    if let Some(l) = &label {
        applied.push(Applied { source: "default".into(), patch: json!({"labels": {"safety_identifier": l}}) });
    }

    let mut body = json!({ "contents": conv.contents });
    if let Some(s) = conv.system {
        body["systemInstruction"] = s;
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if !tool_config.is_empty() {
        body["toolConfig"] = Value::Object(tool_config);
    }
    if !generation.is_empty() {
        body["generationConfig"] = Value::Object(generation);
    }
    if let Some(t) = &tier {
        // Google's REST samples spell it `service_tier` (proto3 JSON also takes `serviceTier`).
        body["service_tier"] = json!(t);
    }
    if let Some(l) = label {
        body["labels"] = json!({ "safety_identifier": l });
    }

    if !spec.known {
        let defaults = match spec.family {
            Family::Gemini4 => format!(
                "Gemini 4 family defaults: thinkingLevel high, long decode up to {} output tokens",
                max_tokens.map(|n| n.to_string()).unwrap_or_else(|| "?".into())
            ),
            Family::Gemini3 => "Gemini 3 family defaults: thinkingLevel high, the API's default maxOutputTokens".into(),
            _ => "only the options given are sent, with the API's default maxOutputTokens".into(),
        };
        warnings.push(format!(
            "`{model}` is not in the bot's Gemini API model table: limits and price unknown (cost not estimated); \
             {defaults}"
        ));
    }

    Ok(Prepared {
        dialect: DIALECT,
        model: model.to_string(),
        url: cfg.url(&format!("/models/{model}:streamGenerateContent?alt=sse")),
        body,
        applied,
        warnings,
        notes: Vec::new(),
        show_debug: false,
        auto_max_tokens: max_tokens,
        long_decode: spec.long_decode,
        settings,
    })
}
