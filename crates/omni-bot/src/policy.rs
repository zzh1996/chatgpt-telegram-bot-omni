//! Request policy, checked on the final request body (so options, presets,
//! model suffixes and provider slugs cannot get around it):
//!
//! - **Providers**: a model may only be served by its vendor's own endpoint or
//!   by official clouds listed for that vendor (`allowed_providers`, e.g.
//!   Google → AI Studio or Vertex; OpenAI → OpenAI or Azure). No OpenRouter
//!   free routing / fallbacks to arbitrary (often quantized) third parties.
//! - **Priority tiers**: everyone, admins included, uses the platform's
//!   default tier unless they ask otherwise; only users in
//!   `[access].priority_users` may ask for a higher-priority (more expensive)
//!   tier. Cheaper/slower tiers (`flex`) are open to everyone.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::app::App;

/// Tiers that cost more than the default (OpenRouter tier endpoints, `service_tier`, Ark `fast`).
const HIGH_TIERS: &[&str] = &["priority", "fast", "ultrafast", "scale"];

/// Official serving options per model author (base provider slugs; regions included).
/// The author's first-party slug is always allowed too.
pub fn default_allowed_providers() -> BTreeMap<String, Vec<String>> {
    let m: &[(&str, &[&str])] = &[
        ("openai", &["openai", "azure", "amazon-bedrock"]),
        ("anthropic", &["anthropic", "amazon-bedrock", "google-vertex", "azure", "claude-on-aws"]),
        ("google", &["google-ai-studio", "google-vertex"]),
        ("x-ai", &["xai", "amazon-bedrock"]),
        ("deepseek", &["deepseek"]),
        ("z-ai", &["z-ai"]),
        ("moonshotai", &["moonshotai"]),
        ("qwen", &["alibaba"]),
        ("bytedance-seed", &["seed"]),
    ];
    m.iter().map(|(a, s)| (a.to_string(), s.iter().map(|x| x.to_string()).collect())).collect()
}

fn base_slug(s: &str) -> &str {
    s.split('/').next().unwrap_or(s)
}

fn high_tier_slug(s: &str) -> bool {
    s.split('/').skip(1).any(|seg| HIGH_TIERS.contains(&seg))
}

/// `Err(message)` if the request is not allowed for this user.
pub fn check(app: &App, user_id: i64, provider: &str, model: &str, body: &Value) -> Result<(), String> {
    let priority_ok = app.cfg.access.priority_users.contains(&user_id);
    let deny_priority = |what: &str| {
        Err(format!(
            "{what} is a higher-priority (more expensive) tier; it is only available to users on the priority list \
             ([access].priority_users). Everyone else uses the default tier"
        ))
    };
    if provider != "openrouter" {
        // The platforms' own APIs: only their priority tiers, in whatever field
        // the platform uses (`service_tier`, Gemini's `serviceTier`, Anthropic's
        // `speed`).
        for key in ["service_tier", "serviceTier"] {
            if let Some(t) = body.get(key).and_then(Value::as_str)
                && HIGH_TIERS.contains(&t.to_ascii_lowercase().as_str())
                && !priority_ok
            {
                return deny_priority(&format!("{key} {t}"));
            }
        }
        if body.get("speed").and_then(Value::as_str) == Some("fast") && !priority_ok {
            return deny_priority("speed fast");
        }
        // Moonshot sells its faster serving as separate models at twice the price
        // (`kimi-k3.1-pro-highspeed`): a priority tier, like OpenRouter's `:nitro`.
        if provider == "moonshot" && model.ends_with("-highspeed") && !priority_ok {
            return deny_priority(&format!("`{model}` (Moonshot's high-speed serving)"));
        }
        return Ok(());
    }
    // Priority tiers, however they are requested.
    if let Some(t) = body.get("service_tier").and_then(Value::as_str)
        && HIGH_TIERS.contains(&t)
        && !priority_ok
    {
        return deny_priority(&format!("service_tier {t}"));
    }
    if body.get("speed").and_then(Value::as_str) == Some("fast") && !priority_ok {
        return deny_priority("speed fast");
    }
    let sent_model = body.get("model").and_then(Value::as_str).unwrap_or(model);
    if sent_model.ends_with(":nitro") && !priority_ok {
        return deny_priority("the :nitro variant");
    }
    let routing = body.get("provider");
    let slugs: Vec<String> = ["only", "order"]
        .iter()
        .filter_map(|k| routing.and_then(|p| p.get(*k)).and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    if let Some(s) = slugs.iter().find(|s| high_tier_slug(s))
        && !priority_ok
    {
        return deny_priority(&format!("provider endpoint {s}"));
    }
    // Providers.
    let author = model.split('/').next().unwrap_or("");
    let mut allowed = app.allowed_providers.get(author).cloned().unwrap_or_default();
    if let Some(fp) = app.openrouter.config().fidelity.first_party_for(model)
        && !allowed.iter().any(|a| a == fp)
    {
        allowed.insert(0, fp.to_string());
    }
    if allowed.is_empty() {
        // No official serving options known for this author: only the default routing.
        if !slugs.is_empty() {
            return Err(format!("choosing a provider is not allowed for `{model}`"));
        }
        return Ok(());
    }
    let pinned = routing.and_then(|p| p.get("only")).and_then(Value::as_array).is_some_and(|a| !a.is_empty());
    let fallbacks = routing.and_then(|p| p.get("allow_fallbacks")).and_then(Value::as_bool) != Some(false);
    if !pinned || fallbacks {
        return Err(format!(
            "`{model}` must be served by one of its official providers ({}); OpenRouter's free routing is not allowed",
            allowed.join(", ")
        ));
    }
    if let Some(bad) = slugs.iter().find(|s| !allowed.iter().any(|a| a == base_slug(s))) {
        return Err(format!(
            "provider `{bad}` is not allowed for `{model}`; allowed: {} (+p=<slug>, regions like google-vertex/us are fine)",
            allowed.join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(base_slug("google-vertex/global/priority"), "google-vertex");
        assert!(high_tier_slug("google-vertex/global/priority"));
        assert!(high_tier_slug("openai/fast"));
        assert!(!high_tier_slug("openai/flex"));
        assert!(!high_tier_slug("azure/eu"));
    }
}
