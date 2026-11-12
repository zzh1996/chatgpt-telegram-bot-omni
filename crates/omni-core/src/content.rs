//! Provider-neutral conversation content model.
//!
//! A conversation is a tree of [`Turn`]s (forkable by replying to any message).
//! Each turn holds:
//! - `parts`: a normalized, ordered list of content parts used for rendering
//!   and for converting the conversation to *another* provider/API;
//! - for assistant turns, an optional [`NativeOutput`]: the exact output items
//!   in the dialect of the API that produced them (e.g. OpenRouter
//!   `reasoning_details` with signatures, OpenAI encrypted reasoning items,
//!   Gemini parts with thought signatures). When the conversation continues on
//!   the same dialect, the native payload is replayed verbatim for maximum
//!   fidelity.
//!
//! Everything is stored as JSON; all fields use `#[serde(default)]` where
//! sensible so that the schema can evolve without migrations.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::blob::BlobRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// Where a turn's content came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// A real Telegram message (user) or a real model generation (assistant).
    #[default]
    Live,
    /// Authored by a user as fake history / system prompt before a conversation.
    Synthetic,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub role: Role,
    #[serde(default)]
    pub origin: Origin,
    #[serde(default)]
    pub parts: Vec<Part>,
    /// Exact provider output (assistant turns only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeOutput>,
}

impl Turn {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self { role: Role::User, origin: Origin::Live, parts: vec![Part::text(text)], native: None }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self { role: Role::Assistant, origin: Origin::Live, parts: vec![Part::text(text)], native: None }
    }

    /// Concatenated visible answer text (no reasoning).
    pub fn answer_text(&self) -> String {
        let mut s = String::new();
        for p in &self.parts {
            if let Part::Text { text, .. } = p {
                s.push_str(text);
            }
        }
        s
    }

    pub fn has_media(&self) -> bool {
        self.parts.iter().any(|p| matches!(p, Part::Image { .. } | Part::File { .. } | Part::Location { .. }))
    }

    /// The last location in this turn, as (latitude, longitude).
    pub fn location(&self) -> Option<(f64, f64)> {
        self.parts.iter().rev().find_map(|p| match p {
            Part::Location { latitude, longitude, .. } => Some((*latitude, *longitude)),
            _ => None,
        })
    }
}

/// Exact output of a provider in its own wire format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeOutput {
    /// API dialect, e.g. `openrouter.chat`, `openai.responses`, `anthropic.messages`, `gemini.generate`.
    pub dialect: String,
    /// The model id as sent upstream (some replay data is only valid for the same model).
    pub model: String,
    /// Dialect-specific payload (e.g. the assistant message object for chat completions).
    pub payload: Value,
    /// Tokens of the whole conversation up to and including this turn, as
    /// reported by the provider for this model (prompt + completion). Used to
    /// budget `max_tokens` of later turns without guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    /// Plain (Markdown) text. For assistant turns this is the model's answer.
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        citations: Vec<Citation>,
    },
    /// An image (input or generated output).
    Image {
        blob: BlobRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        /// True if produced by the model (or a server tool) rather than the user.
        #[serde(default)]
        generated: bool,
        /// Pixel dimensions (width, height), when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dims: Option<(u32, u32)>,
    },
    /// Any other file: PDF, audio, video, text/code documents, ...
    /// Mapping to the provider's input type (native document, inline data,
    /// inlined text, ...) is decided per request by the provider adapter.
    File {
        blob: BlobRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        #[serde(default)]
        generated: bool,
    },
    /// A remote resource referenced by URL that the provider fetches itself
    /// (e.g. a YouTube video for Gemini). Never fetched locally.
    RemoteMedia { url: String, kind: RemoteMediaKind },
    /// A position shared in Telegram (location, venue or live location). It is
    /// input for location-aware tools (Gemini's Google Maps grounding takes the
    /// latest one as the user's position); APIs without such input reject it.
    Location {
        latitude: f64,
        longitude: f64,
        /// Horizontal accuracy in meters, when Telegram reports one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accuracy_m: Option<u32>,
        /// Venue name and address (venue messages).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<String>,
    },
    /// Model reasoning (raw thinking, summary, encrypted or redacted).
    Reasoning(Reasoning),
    /// A server-side (built-in, provider-executed) tool invocation and its result.
    ToolCall(ToolCall),
    /// Model refusal message (OpenAI `refusal` content etc.).
    Refusal { text: String },
}

impl Part {
    pub fn text(t: impl Into<String>) -> Self {
        Part::Text { text: t.into(), citations: Vec::new() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteMediaKind {
    YouTube,
    Video,
    Image,
    Document,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningKind {
    /// Full raw reasoning text as produced by the model.
    #[default]
    Text,
    /// A provider-generated summary of the reasoning.
    Summary,
    /// Opaque encrypted reasoning (no visible text).
    Encrypted,
    /// Reasoning redacted by the provider's safety systems.
    Redacted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Reasoning {
    #[serde(default)]
    pub kind: ReasoningKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Signature / encrypted content, needed for replay by some providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opaque: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Provider format tag (e.g. OpenRouter `format`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ToolCall {
    /// Provider-assigned id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Tool name as reported by the provider, e.g. `web_search`, `code_interpreter`.
    pub name: String,
    /// Status as reported (e.g. `completed`, `failed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Human-relevant input (search query, code, URL...), verbatim from the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    /// Human-relevant output (stdout, result summary...), verbatim from the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
}

/// A citation attached to a text part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Citation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Quoted source text, when the provider returns it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cited_text: Option<String>,
    /// Byte range in the containing text part that this citation supports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<(usize, usize)>,
    /// Anything else the provider returned, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_json() {
        let t = Turn {
            role: Role::Assistant,
            origin: Origin::Live,
            parts: vec![
                Part::Reasoning(Reasoning {
                    kind: ReasoningKind::Summary,
                    text: Some("thinking".into()),
                    ..Default::default()
                }),
                Part::text("answer"),
            ],
            native: Some(NativeOutput {
                dialect: "openrouter.chat".into(),
                model: "x/y".into(),
                payload: serde_json::json!({"role": "assistant", "content": "answer"}),
                context_tokens: Some(12),
            }),
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: Turn = serde_json::from_str(&s).unwrap();
        assert_eq!(t, back);
        assert_eq!(back.answer_text(), "answer");
    }

    #[test]
    fn tolerant_of_missing_fields() {
        let t: Turn = serde_json::from_str(r#"{"role":"user","parts":[{"type":"text","text":"hi"}]}"#).unwrap();
        assert_eq!(t.origin, Origin::Live);
        assert_eq!(t.parts, vec![Part::text("hi")]);
    }
}
