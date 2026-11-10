//! Parsing of the user-facing trigger syntax.
//!
//! ```text
//! <specs>$<text>
//! specs  := spec (',' spec)*              -- several specs = several models in parallel
//! spec   := model ('+' option)*
//! model  := alias | provider model id     -- e.g. `c`, `anthropic/claude-opus-5-5`, `or:x-ai/grok-5`
//! option := flag | key=value | {json}     -- e.g. `s`, `effort=low`, `{"temperature":0.2}`
//! ```
//!
//! The prefix must be on the first line. `{...}` JSON objects may contain
//! `,`, `+` and `$`; the scanner is JSON-string aware.
//!
//! The text after `$` may define a synthetic history with role marker lines
//! (`::system`, `::user`, `::assistant`), see [`parse_history`].

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::content::Role;

#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    pub specs: Vec<SpecToken>,
    /// Message text after the `$` (leading whitespace removed).
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpecToken {
    /// Alias or model id; empty means "the default model".
    pub model: String,
    pub options: Vec<OptionToken>,
    /// The spec exactly as typed (for display / error messages).
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum OptionToken {
    Flag(String),
    KeyValue(String, String),
    Json(Value),
}

impl OptionToken {
    /// Render back to the trigger syntax (without the leading `+`).
    pub fn to_syntax(&self) -> String {
        match self {
            OptionToken::Flag(f) => f.clone(),
            OptionToken::KeyValue(k, v) => format!("{k}={v}"),
            OptionToken::Json(v) => v.to_string(),
        }
    }
}

/// Parse a standalone spec string such as an alias definition
/// (`openrouter:anthropic/claude-opus-5.5+s+effort=max`).
pub fn parse_spec(s: &str) -> Result<SpecToken, TriggerError> {
    let t = parse_trigger(&format!("{s}$"))?.ok_or(TriggerError::EmptySpec)?;
    let mut specs = t.specs;
    if specs.len() != 1 {
        return Err(TriggerError::EmptySpec);
    }
    Ok(specs.remove(0))
}

#[derive(Debug, Error, PartialEq)]
pub enum TriggerError {
    #[error("empty model spec in prefix")]
    EmptySpec,
    #[error("invalid JSON option `{0}`: {1}")]
    BadJson(String, String),
    #[error("JSON option must be an object: `{0}`")]
    JsonNotObject(String),
    #[error("unbalanced braces in prefix")]
    Unbalanced,
    #[error("empty option in `{0}`")]
    EmptyOption(String),
}

/// Find the trigger `$` on the first line, honoring `{...}` JSON segments.
/// Returns the byte index of the `$`.
pub fn find_trigger_dollar(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, c) in text.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '\n' if depth == 0 => return None,
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            '"' if depth > 0 => in_str = true,
            '$' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// Split `s` on `sep` at JSON depth 0.
fn split_top(s: &str, sep: char) -> Result<Vec<&str>, TriggerError> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    let mut last = 0;
    for (i, c) in s.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth < 0 {
                    return Err(TriggerError::Unbalanced);
                }
            }
            '"' if depth > 0 => in_str = true,
            c if c == sep && depth == 0 => {
                out.push(&s[last..i]);
                last = i + c.len_utf8();
            }
            _ => {}
        }
    }
    if depth != 0 || in_str {
        return Err(TriggerError::Unbalanced);
    }
    out.push(&s[last..]);
    Ok(out)
}

/// Parse a message that may start with a trigger prefix.
///
/// Returns `Ok(None)` if the text does not have a syntactically valid prefix
/// shape (no `$` on the first line). Whether the model tokens actually resolve
/// is decided by the caller.
pub fn parse_trigger(text: &str) -> Result<Option<Trigger>, TriggerError> {
    let Some(pos) = find_trigger_dollar(text) else { return Ok(None) };
    let prefix = &text[..pos];
    let rest = &text[pos + 1..];
    let mut specs = Vec::new();
    for raw_spec in split_top(prefix, ',')? {
        let raw_spec = raw_spec.trim();
        if raw_spec.is_empty() {
            if prefix.trim().is_empty() {
                // `$text`: default model, no options.
                specs.push(SpecToken { model: String::new(), options: Vec::new(), raw: String::new() });
                continue;
            }
            return Err(TriggerError::EmptySpec);
        }
        let mut parts = split_top(raw_spec, '+')?.into_iter();
        let model = parts.next().unwrap_or("").trim().to_string();
        let mut options = Vec::new();
        for opt in parts {
            let opt = opt.trim();
            if opt.is_empty() {
                return Err(TriggerError::EmptyOption(raw_spec.to_string()));
            }
            if opt.starts_with('{') {
                let v: Value =
                    serde_json::from_str(opt).map_err(|e| TriggerError::BadJson(opt.to_string(), e.to_string()))?;
                if !v.is_object() {
                    return Err(TriggerError::JsonNotObject(opt.to_string()));
                }
                options.push(OptionToken::Json(v));
            } else if let Some((k, v)) = opt.split_once('=') {
                options.push(OptionToken::KeyValue(k.trim().to_string(), v.trim().to_string()));
            } else {
                options.push(OptionToken::Flag(opt.to_string()));
            }
        }
        specs.push(SpecToken { model, options, raw: raw_spec.to_string() });
    }
    // The text after `$` is sent exactly as typed: `c$hi` sends "hi", while
    // `c$ hi` sends " hi" (a leading space is deliberate).
    Ok(Some(Trigger { specs, text: rest.to_string() }))
}

/// A segment of user-authored history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySegment {
    pub role: Role,
    pub text: String,
}

/// Parse role marker lines (`::system`, `::user`, `::assistant`) in a message.
///
/// Returns `None` if the text contains no marker line (the whole text is a
/// single user message). Text before the first marker becomes a user segment
/// if it is not blank. Marker lines are matched case-insensitively and must be
/// alone on their line.
pub fn parse_history(text: &str) -> Option<Vec<HistorySegment>> {
    let marker = |line: &str| -> Option<Role> {
        let l = line.trim();
        let name = l.strip_prefix("::")?;
        match name.to_ascii_lowercase().as_str() {
            "system" | "sys" => Some(Role::System),
            "user" | "u" => Some(Role::User),
            "assistant" | "a" | "model" => Some(Role::Assistant),
            _ => None,
        }
    };
    if !text.lines().any(|l| marker(l).is_some()) {
        return None;
    }
    let mut segs: Vec<HistorySegment> = Vec::new();
    let mut cur_role = Role::User;
    let mut cur = String::new();
    let mut started = false;
    for line in text.split_inclusive('\n') {
        if let Some(r) = marker(line.trim_end_matches(['\n', '\r'])) {
            if started || !cur.trim().is_empty() {
                segs.push(HistorySegment { role: cur_role, text: trim_segment(&cur) });
            }
            cur.clear();
            cur_role = r;
            started = true;
        } else {
            cur.push_str(line);
        }
    }
    segs.push(HistorySegment { role: cur_role, text: trim_segment(&cur) });
    Some(segs)
}

/// Remove the line breaks directly adjacent to marker lines, keep inner text verbatim.
fn trim_segment(s: &str) -> String {
    let s = s.strip_suffix('\n').unwrap_or(s);
    let s = s.strip_suffix('\r').unwrap_or(s);
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn spec(model: &str, options: Vec<OptionToken>, raw: &str) -> SpecToken {
        SpecToken { model: model.into(), options, raw: raw.into() }
    }

    #[test]
    fn simple() {
        let t = parse_trigger("c$hello").unwrap().unwrap();
        assert_eq!(t.specs, vec![spec("c", vec![], "c")]);
        assert_eq!(t.text, "hello");
        // Whitespace after `$` is part of the message.
        assert_eq!(parse_trigger("c$ hello").unwrap().unwrap().text, " hello");
        assert_eq!(parse_trigger("c$\nhello").unwrap().unwrap().text, "\nhello");
    }

    #[test]
    fn default_model() {
        let t = parse_trigger("$hello $5").unwrap().unwrap();
        assert_eq!(t.specs, vec![spec("", vec![], "")]);
        assert_eq!(t.text, "hello $5");
    }

    #[test]
    fn multi_and_options() {
        let t = parse_trigger("c+s+effort=low, g+4k+ar=16:9 ,openai/gpt-6$ hi").unwrap().unwrap();
        assert_eq!(
            t.specs,
            vec![
                spec(
                    "c",
                    vec![OptionToken::Flag("s".into()), OptionToken::KeyValue("effort".into(), "low".into())],
                    "c+s+effort=low"
                ),
                spec(
                    "g",
                    vec![OptionToken::Flag("4k".into()), OptionToken::KeyValue("ar".into(), "16:9".into())],
                    "g+4k+ar=16:9"
                ),
                spec("openai/gpt-6", vec![], "openai/gpt-6"),
            ]
        );
    }

    #[test]
    fn json_option_with_specials() {
        let t = parse_trigger(r#"c+{"stop":["$","a,b+c"],"x":{"y":1}}$go"#).unwrap().unwrap();
        assert_eq!(t.specs.len(), 1);
        match &t.specs[0].options[0] {
            OptionToken::Json(v) => assert_eq!(v["stop"][0], "$"),
            o => panic!("{o:?}"),
        }
        assert_eq!(t.text, "go");
    }

    #[test]
    fn not_a_trigger() {
        assert_eq!(parse_trigger("hello world").unwrap(), None);
        assert_eq!(parse_trigger("first line\nc$ second").unwrap(), None);
    }

    #[test]
    fn errors() {
        assert_eq!(parse_trigger("a,,b$ x"), Err(TriggerError::EmptySpec));
        assert_eq!(parse_trigger("a++s$ x"), Err(TriggerError::EmptyOption("a++s".into())));
        assert!(matches!(parse_trigger("a+{bad}$ x"), Err(TriggerError::BadJson(..))));
        assert!(matches!(parse_trigger("a+[1]$ x"), Ok(Some(_)))); // `[1]` is just a flag name
    }

    #[test]
    fn history_markers() {
        let h = parse_history("::system\nYou are a cat.\n::user\nhi\n::assistant\nmeow\n::user\nwho are you?").unwrap();
        assert_eq!(
            h,
            vec![
                HistorySegment { role: Role::System, text: "You are a cat.".into() },
                HistorySegment { role: Role::User, text: "hi".into() },
                HistorySegment { role: Role::Assistant, text: "meow".into() },
                HistorySegment { role: Role::User, text: "who are you?".into() },
            ]
        );
        assert_eq!(parse_history("no markers ::user inline"), None);
        let h = parse_history("leading text\n::assistant\nprefill").unwrap();
        assert_eq!(h[0], HistorySegment { role: Role::User, text: "leading text".into() });
        assert_eq!(h[1], HistorySegment { role: Role::Assistant, text: "prefill".into() });
    }
}
