//! Parsing of the user-facing trigger syntax.
//!
//! ```text
//! <specs><marker><text>                   -- `$` is the default marker: `c$hello`
//! specs  := spec (',' spec)*              -- several specs = several models in parallel
//! spec   := model ('+' option)*
//! model  := alias | model id               -- e.g. `c`, `` (the empty alias), `anthropic/claude-opus-5.5`
//! option := flag | key=value               -- e.g. `s`, `4W`, `e=low`
//! ```
//!
//! The marker is configurable per deployment (see [`check_marker`] for what is
//! allowed). The prefix must be on the first line. A `{...}` JSON object is still
//! read as one option (it may contain `,`, `+` and the marker; the scanner is
//! JSON-string aware) only so that the options parser can reject it clearly.
//!
//! The text after the marker may define a synthetic history with role marker
//! lines (`::system`, `::user`, `::assistant`), see [`parse_history`].

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::content::Role;

#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    pub specs: Vec<SpecToken>,
    /// Message text after the marker, exactly as typed.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpecToken {
    /// Alias or model id; may be empty (the alias with the empty name).
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

/// The default marker that ends the prefix: `c$hello`.
pub const DEFAULT_MARKER: &str = "$";

/// Characters that prefixes are made of (besides letters and digits), so a
/// marker can't use them; plus `\` (escapes), `@` (mentions) and the backtick
/// (Telegram clients turn backticks into code formatting).
const PREFIX_CHARS: &str = ",+{}\"=/-.:_\\@`";

/// Whether `marker` can end a prefix: it must be non-empty and use no letter,
/// digit, whitespace or control character and none of ``,+{}"=/-.:_\@` ``.
/// Any other characters work, also several (`%`, `¥`, `>>`, `：`).
pub fn check_marker(marker: &str) -> Result<(), String> {
    if marker.is_empty() {
        return Err("the trigger marker is empty".into());
    }
    if let Some(c) =
        marker.chars().find(|c| c.is_alphanumeric() || c.is_whitespace() || c.is_control() || PREFIX_CHARS.contains(*c))
    {
        return Err(format!("the trigger marker {marker:?} contains {c:?}, which prefixes themselves use"));
    }
    Ok(())
}

/// Whether `name` can be an alias next to `marker`: without the marker,
/// whitespace or the separators `,+{}`. The empty name is an alias like any
/// other (`$text`).
pub fn alias_name_ok(name: &str, marker: &str) -> bool {
    !name.contains(marker) && !name.contains(|c: char| c.is_whitespace() || ",+{}".contains(c))
}

/// Parse a standalone spec string such as an alias definition
/// (`openrouter:anthropic/claude-opus-5.5+s+e=max`).
pub fn parse_spec(s: &str) -> Result<SpecToken, TriggerError> {
    if s.contains('\n') {
        return Err(TriggerError::EmptySpec);
    }
    let mut specs = parse_specs(s)?;
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

/// Find `marker` on the first line, honoring `{...}` JSON segments.
/// Returns its byte index.
pub fn find_marker(text: &str, marker: &str) -> Option<usize> {
    if marker.is_empty() {
        return None;
    }
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
            _ if depth == 0 && text[i..].starts_with(marker) => return Some(i),
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

/// Parse a message that may start with a trigger prefix ending in `marker`.
///
/// Returns `Ok(None)` if the text does not have a syntactically valid prefix
/// shape (no marker on the first line). Whether the model tokens actually
/// resolve is decided by the caller.
pub fn parse_trigger(text: &str, marker: &str) -> Result<Option<Trigger>, TriggerError> {
    let Some(pos) = find_marker(text, marker) else { return Ok(None) };
    let specs = parse_specs(&text[..pos])?;
    // The text after the marker is sent exactly as typed: `c$hi` sends "hi",
    // while `c$ hi` sends " hi" (a leading space is deliberate).
    Ok(Some(Trigger { specs, text: text[pos + marker.len()..].to_string() }))
}

/// The comma-separated specs of a prefix.
fn parse_specs(prefix: &str) -> Result<Vec<SpecToken>, TriggerError> {
    let mut specs = Vec::new();
    for raw_spec in split_top(prefix, ',')? {
        let raw_spec = raw_spec.trim();
        if raw_spec.is_empty() {
            // An empty spec is the alias with the empty name: `$text`, `,c$text`, `c,,g$text`.
            specs.push(SpecToken { model: String::new(), options: Vec::new(), raw: String::new() });
            continue;
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
    Ok(specs)
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
    if !text.lines().any(|l| marker_role(l).is_some()) {
        return None;
    }
    let mut segs: Vec<HistorySegment> = Vec::new();
    let mut cur_role = Role::User;
    let mut cur = String::new();
    let mut started = false;
    for line in text.split_inclusive('\n') {
        if let Some(r) = marker_role(line.trim_end_matches(['\n', '\r'])) {
            if started || !cur.trim().is_empty() {
                segs.push(HistorySegment { role: cur_role, text: trim_segment(&cur) });
            }
            cur.clear();
            cur_role = r;
            started = true;
        } else {
            cur.push_str(&unescape_line(line));
        }
    }
    segs.push(HistorySegment { role: cur_role, text: trim_segment(&cur) });
    Some(segs)
}

/// A role marker line: `::system` (`::sys`), `::user` (`::u`), `::assistant`
/// (`::a`, `::model`), any case, alone on its line (spaces around allowed).
fn marker_role(line: &str) -> Option<Role> {
    let name = line.trim().strip_prefix("::")?;
    match name.to_ascii_lowercase().as_str() {
        "system" | "sys" => Some(Role::System),
        "user" | "u" => Some(Role::User),
        "assistant" | "a" | "model" => Some(Role::Assistant),
        _ => None,
    }
}

/// A marker line written with backslashes in front (`\::user`, `\\::user`) is
/// text: it is sent with one backslash less, so that any text can be sent.
fn unescape_line(line: &str) -> std::borrow::Cow<'_, str> {
    let body = line.trim_start();
    let lead = &line[..line.len() - body.len()];
    match body.strip_prefix('\\') {
        Some(rest) if marker_role(rest.trim_start_matches('\\').trim_end_matches(['\n', '\r'])).is_some() => {
            format!("{lead}{rest}").into()
        }
        _ => line.into(),
    }
}

/// Text without marker lines, with escaped marker lines unescaped (see
/// [`parse_history`]).
pub fn unescape_markers(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.lines().any(|l| l.trim_start().starts_with("\\")) {
        return text.into();
    }
    text.split_inclusive('\n').map(unescape_line).collect::<String>().into()
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
        let t = parse_trigger("c$hello", "$").unwrap().unwrap();
        assert_eq!(t.specs, vec![spec("c", vec![], "c")]);
        assert_eq!(t.text, "hello");
        // Whitespace after `$` is part of the message.
        assert_eq!(parse_trigger("c$ hello", "$").unwrap().unwrap().text, " hello");
        assert_eq!(parse_trigger("c$\nhello", "$").unwrap().unwrap().text, "\nhello");
    }

    #[test]
    fn empty_prefix() {
        let t = parse_trigger("$hello $5", "$").unwrap().unwrap();
        assert_eq!(t.specs, vec![spec("", vec![], "")]);
        assert_eq!(t.text, "hello $5");
    }

    #[test]
    fn multi_and_options() {
        let t = parse_trigger("c+s+effort=low, g+4k+ar=16:9 ,openai/gpt-6$ hi", "$").unwrap().unwrap();
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
        let t = parse_trigger(r#"c+{"stop":["$","a,b+c"],"x":{"y":1}}$go"#, "$").unwrap().unwrap();
        assert_eq!(t.specs.len(), 1);
        match &t.specs[0].options[0] {
            OptionToken::Json(v) => assert_eq!(v["stop"][0], "$"),
            o => panic!("{o:?}"),
        }
        assert_eq!(t.text, "go");
    }

    #[test]
    fn not_a_trigger() {
        assert_eq!(parse_trigger("hello world", "$").unwrap(), None);
        assert_eq!(parse_trigger("first line\nc$ second", "$").unwrap(), None);
    }

    #[test]
    fn empty_specs_are_the_empty_alias() {
        let models = |s: &str| -> Vec<String> {
            parse_trigger(s, "$").unwrap().unwrap().specs.into_iter().map(|s| s.model).collect()
        };
        assert_eq!(models("$hi"), vec![""]);
        assert_eq!(models(",c$hi"), vec!["", "c"]);
        assert_eq!(models("c,,g$hi"), vec!["c", "", "g"]);
        assert_eq!(models("c,$hi"), vec!["c", ""]);
        assert_eq!(models(" , c+s $hi"), vec!["", "c"]);
        let t = parse_trigger("c,,g$hi", "$").unwrap().unwrap();
        assert!(t.specs[1].options.is_empty() && t.specs[1].raw.is_empty());
        assert_eq!(t.text, "hi");
    }

    #[test]
    fn errors() {
        assert_eq!(parse_trigger("a++s$ x", "$"), Err(TriggerError::EmptyOption("a++s".into())));
        assert!(matches!(parse_trigger("a+{bad}$ x", "$"), Err(TriggerError::BadJson(..))));
        assert!(matches!(parse_trigger("a+[1]$ x", "$"), Ok(Some(_)))); // `[1]` is just a flag name
    }

    #[test]
    fn other_markers() {
        // `%`: `$` is plain text then, `%` ends the prefix (also inside prose after it).
        let t = parse_trigger("c,,g+s%costs 5$ or 5%", "%").unwrap().unwrap();
        assert_eq!(t.specs.iter().map(|s| s.model.as_str()).collect::<Vec<_>>(), vec!["c", "", "g"]);
        assert_eq!(t.text, "costs 5$ or 5%");
        assert_eq!(parse_trigger("c$hi", "%").unwrap(), None);
        // Several characters, non-ASCII, and the marker inside JSON strings.
        let t = parse_trigger(r#"c+{"stop":[">>"]}>> hi"#, ">>").unwrap().unwrap();
        assert_eq!(t.text, " hi");
        assert_eq!(parse_trigger("¥hi", "¥").unwrap().unwrap().specs, vec![spec("", vec![], "")]);
        assert_eq!(parse_trigger("c：你好", "：").unwrap().unwrap().text, "你好");
        // A spec does not depend on the marker.
        assert_eq!(parse_spec("c+s").unwrap(), spec("c", vec![OptionToken::Flag("s".into())], "c+s"));
    }

    #[test]
    fn alias_names() {
        assert!(alias_name_ok("g5", "$") && alias_name_ok("g$5", "%") && alias_name_ok("x/y:z", "$"));
        assert!(alias_name_ok("", "$"));
        for bad in ["a$", "$", "a b", "a,b", "a+b", "{a}"] {
            assert!(!alias_name_ok(bad, "$"), "{bad:?}");
        }
        assert!(!alias_name_ok("a>>b", ">>"));
    }

    #[test]
    fn marker_check() {
        for ok in ["$", "%", "¥", ">>", "：", "#", "!", "→"] {
            assert_eq!(check_marker(ok), Ok(()), "{ok}");
        }
        for bad in ["", "a", "5", "问", " ", "$ ", ",", "+", "{", "\"", "=", "/", "-", ".", ":", "_", "\\", "@", "`", "\n"] {
            assert!(check_marker(bad).is_err(), "{bad:?}");
        }
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
        // Escaped marker lines are text, one backslash less.
        let h = parse_history("::user\n\\::assistant\n  \\\\::system\n::assistant\nok").unwrap();
        assert_eq!(h[0].text, "::assistant\n  \\::system");
        assert_eq!(unescape_markers("a\n\\::user\nb \\::user\n\\x"), "a\n::user\nb \\::user\n\\x");
        assert_eq!(parse_history("\\::user only"), None);
        let h = parse_history("leading text\n::assistant\nprefill").unwrap();
        assert_eq!(h[0], HistorySegment { role: Role::User, text: "leading text".into() });
        assert_eq!(h[1], HistorySegment { role: Role::Assistant, text: "prefill".into() });
    }
}
