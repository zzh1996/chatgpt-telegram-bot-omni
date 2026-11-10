//! Human-readable summary of what a prepared request actually asks for — shown
//! under each reply so users can check that their options were understood.
//!
//! Derived from the final request body (not from the option syntax), so it
//! shows what was really sent: the reasoning setting always, plus every field
//! set by user options, JSON patches or config presets.

use serde_json::Value;

use crate::Prepared;

/// Short descriptions, e.g. `["effort high", "web search (native)", "image size 1K"]`.
pub fn describe(p: &Prepared) -> Vec<String> {
    let body = &p.body;
    // Top-level fields set by the user or presets (not by the automatic defaults).
    let mut touched: Vec<String> = Vec::new();
    for a in &p.applied {
        let s = a.source.as_str();
        if (s.starts_with("option:") || s == "json" || s.starts_with("preset:"))
            && let Some(o) = a.patch.as_object() {
                for k in o.keys() {
                    if !touched.contains(k) {
                        touched.push(k.clone());
                    }
                }
            }
    }
    let mut out = Vec::new();
    // Reasoning: always.
    if let Some(r) = reasoning(body) {
        out.push(r);
    }
    for k in &touched {
        if matches!(k.as_str(), "reasoning" | "reasoning_effort" | "thinking") {
            continue; // covered above
        }
        if let Some(d) = field(k, body.get(k)) {
            out.push(d);
        }
    }
    out
}

fn reasoning(body: &Value) -> Option<String> {
    // OpenRouter Chat Completions
    if let Some(r) = body.get("reasoning").filter(|v| v.is_object()) {
        let mut parts = Vec::new();
        let enabled = r.get("enabled").and_then(Value::as_bool);
        match r.get("effort").and_then(Value::as_str) {
            Some("none") => parts.push("reasoning off".to_string()),
            Some(e) => parts.push(format!("effort {e}")),
            None => match (r.get("max_tokens").and_then(Value::as_u64), enabled) {
                (Some(n), _) => parts.push(format!("thinking budget {n}")),
                (None, Some(false)) => parts.push("reasoning off".into()),
                (None, Some(true)) => parts.push("reasoning on".into()),
                _ => {}
            },
        }
        if r.get("effort").is_some()
            && let Some(n) = r.get("max_tokens").and_then(Value::as_u64)
        {
            parts.push(format!("thinking budget {n}"));
        }
        if let Some(s) = r.get("summary").and_then(Value::as_str) {
            parts.push(format!("summary {s}"));
        }
        if r.get("exclude").and_then(Value::as_bool) == Some(true) {
            parts.push("reasoning hidden".into());
        }
        return (!parts.is_empty()).then(|| parts.join(", "));
    }
    // Volcengine Ark
    let off = body.pointer("/thinking/type").and_then(Value::as_str) == Some("disabled");
    match (off, body.get("reasoning_effort").and_then(Value::as_str)) {
        (true, _) | (false, Some("minimal" | "none")) => Some("thinking off".into()),
        (false, Some(e)) => Some(format!("effort {e}")),
        (false, None) => None,
    }
}

fn compact(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if s.chars().count() > 60 { format!("{}…", s.chars().take(60).collect::<String>()) } else { s }
}

fn field(k: &str, v: Option<&Value>) -> Option<String> {
    let Some(v) = v else {
        return Some(format!("{k} removed"));
    };
    Some(match k {
        "tools" => {
            let names: Vec<String> = v
                .as_array()?
                .iter()
                .map(|t| match t.get("type").and_then(Value::as_str) {
                    Some("openrouter:web_search") => {
                        let engine = t.pointer("/parameters/engine").and_then(Value::as_str).unwrap_or("auto");
                        format!("web search ({engine})")
                    }
                    Some(ty) => ty.to_string(),
                    None => compact(t),
                })
                .collect();
            names.join(", ")
        }
        "web_search_options" => "Doubao search".into(),
        "image_config" => {
            let mut p = Vec::new();
            if let Some(s) = v.get("image_size").and_then(Value::as_str) {
                p.push(format!("image size {s}"));
            }
            if let Some(a) = v.get("aspect_ratio").and_then(Value::as_str) {
                p.push(format!("aspect ratio {a}"));
            }
            for (k2, v2) in v.as_object()? {
                if k2 != "image_size" && k2 != "aspect_ratio" {
                    p.push(format!("image {k2} {}", compact(v2)));
                }
            }
            p.join(", ")
        }
        "modalities" => {
            let m: Vec<&str> = v.as_array()?.iter().filter_map(Value::as_str).collect();
            format!("output {}", m.join("+"))
        }
        "response_format" => match v.get("type").and_then(Value::as_str) {
            Some("json_object") => "JSON output".into(),
            Some("json_schema") => "JSON schema output".into(),
            _ => format!("response_format {}", compact(v)),
        },
        "provider" => {
            if let Some(only) = v.get("only").and_then(Value::as_array) {
                let s: Vec<&str> = only.iter().filter_map(Value::as_str).collect();
                format!("provider {}", s.join("|"))
            } else if v.get("allow_fallbacks").and_then(Value::as_bool) != Some(false) {
                "any provider (OpenRouter routing)".into()
            } else {
                format!("provider {}", compact(v))
            }
        }
        "max_tokens" => format!("max_tokens {}", compact(v)),
        "service_tier" => format!("tier {}", compact(v)),
        "debug" => "upstream body echo".into(),
        "plugins" => return None, // fidelity plumbing
        other => format!("{other} {}", compact(v)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openrouter::options::Applied;
    use serde_json::json;

    fn prepared(body: Value, applied: Vec<(&str, Value)>) -> Prepared {
        Prepared {
            dialect: "openrouter.chat",
            model: "m/x".into(),
            url: String::new(),
            body,
            applied: applied.into_iter().map(|(s, p)| Applied { source: s.into(), patch: p }).collect(),
            warnings: vec![],
            notes: vec![],
            show_debug: false,
            auto_max_tokens: None,
            long_decode: false,
        }
    }

    #[test]
    fn effort_always_and_options_expanded() {
        let body = json!({
            "reasoning": {"effort": "high"},
            "tools": [{"type": "openrouter:web_search", "parameters": {"engine": "native"}}],
            "image_config": {"image_size": "1K"},
            "temperature": 0.2,
            "max_tokens": 1000,
        });
        let p = prepared(
            body,
            vec![
                ("auto-max", json!({"reasoning": {"effort": "high"}, "max_tokens": 1000})),
                ("option:s", json!({"tools": []})),
                ("preset:x", json!({"image_config": {}})),
                ("json", json!({"temperature": 0.2})),
            ],
        );
        assert_eq!(describe(&p), vec!["effort high", "web search (native)", "image size 1K", "temperature 0.2"]);
    }

    #[test]
    fn ark_thinking() {
        let p = prepared(json!({"thinking": {"type": "disabled"}, "reasoning_effort": "minimal"}), vec![]);
        assert_eq!(describe(&p), vec!["thinking off"]);
    }
}
