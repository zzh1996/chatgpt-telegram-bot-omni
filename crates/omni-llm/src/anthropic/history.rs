//! The conversation in Messages API form: `system`, `messages`, and notes on
//! how it was converted.
//!
//! - Leading system turns become the top-level `system`; a system turn after
//!   the conversation started is sent in place as a `role: "system"` message
//!   (only models that take those; the API checks where it may stand).
//! - An assistant turn made by this API, model and tool set is replayed as its
//!   content blocks, exactly as received (thinking with signatures, empty
//!   ones included, redacted thinking, server tool calls and results with
//!   their encrypted content, text with citations), split into the requests
//!   that produced it (pause_turn continuations). Anything else is sent as its
//!   text. The tool set is part of the condition because the API checks every
//!   replayed thinking block against the `system`, `tools` and messages that
//!   preceded it (preserved thinking, docs/research/anthropic.md §2.2): a turn
//!   made with other tools would fail that check, and sending it as text keeps
//!   every request's prefix identical to the one each replayed block was made
//!   with. Images and files generated in an answer sent as text cannot go
//!   with it (assistant messages carry the model's own content blocks; the
//!   docs show no media in them, and a `document` there is a 400 since API
//!   version 2026-10-21): a warning says what was left out (nothing is moved
//!   into a user message).
//! - User content: text; images (JPEG, PNG, GIF, WebP) and PDFs as base64;
//!   text files as plain-text `document` blocks (the API's own text-document
//!   input, so nothing is inlined, D6). Other media are errors.
//! - Citations: every document has `citations: {enabled: true}` (the API
//!   wants all or none), from a conversation's first turn on, so earlier
//!   messages never change ([`citations_allowed`]: a conversation whose
//!   earlier answers were made without them keeps them off). The native
//!   payload records the setting (`citations`) and the message id (`id`, for
//!   the cache diagnostics of the next request, [`previous_message_id`]).

use omni_core::content::{Part, Role, Turn};
use serde_json::{Value, json};

use super::models::ModelSpec;
use super::{DIALECT, NAME};
use crate::error::LlmError;
use crate::provider::{b64, load, no_location};
use crate::{BlobLoader, MediaClass, classify_mime};

/// Image types the Messages API takes.
const IMAGE_TYPES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];

#[derive(Debug, Default)]
pub struct Converted {
    pub system: Option<Value>,
    pub messages: Vec<Value>,
    pub notes: Vec<String>,
    /// What could not be sent (generated media of answers sent as text).
    pub warnings: Vec<String>,
    /// The user messages have documents.
    pub documents: bool,
}

enum Replay {
    Native(Vec<Value>),
    /// Same model, but the turn was made with another tool set.
    OtherTools,
    Text,
}

/// Convert `history` for `model`. `tools`: the request's `tools` (replayed
/// turns must have been made with the same ones); `cite`: documents get
/// citations ([`citations_allowed`]).
pub async fn convert(
    history: &[Turn],
    model: &str,
    spec: Option<&ModelSpec>,
    tools: Option<&Value>,
    cite: bool,
    blobs: &dyn BlobLoader,
) -> Result<Converted, LlmError> {
    let mut system: Vec<Value> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    let mut started = false;
    let mut other_tools = false;
    let mut as_text: Vec<&Turn> = Vec::new();
    let mut documents = false;
    for turn in history {
        match turn.role {
            Role::System => {
                let text = turn.answer_text();
                if text.is_empty() {
                    continue;
                }
                let block = json!({"type": "text", "text": text});
                if !started {
                    system.push(block);
                } else if spec.is_some_and(|s| !s.mid_system) {
                    return Err(LlmError::request(format!(
                        "`{model}` takes a system prompt only at the start of the conversation \
                         (no system messages later on)"
                    )));
                } else {
                    messages.push(json!({"role": "system", "content": [block]}));
                }
            }
            Role::Assistant => {
                started = true;
                match replay(turn, model, tools) {
                    Replay::Native(msgs) => messages.extend(msgs),
                    other => {
                        other_tools |= matches!(other, Replay::OtherTools);
                        as_text.push(turn);
                        let text = turn.answer_text();
                        if text.is_empty() {
                            let media = media_not_sent([turn], ASSISTANT_TEXT_ONLY)
                                .map(|m| format!("; {m}"))
                                .unwrap_or_default();
                            return Err(LlmError::request(format!(
                                "an earlier answer in this conversation has no text to give `{model}` \
                                 (its thinking and tool calls go back only to the model that made them, \
                                 with the same tools{media})"
                            )));
                        }
                        messages.push(json!({"role": "assistant", "content": [{"type": "text", "text": text}]}));
                    }
                }
            }
            Role::User => {
                started = true;
                let content = user_content(turn, model, cite, blobs).await?;
                documents |= content.iter().any(|b| b["type"] == "document");
                messages.push(json!({"role": "user", "content": content}));
            }
        }
    }
    let mut notes = Vec::new();
    if other_tools {
        notes.push(
            "earlier answers made with other tools are sent as plain text (Anthropic binds their thinking \
             and tool results to the tools of the request that made them)"
                .to_string(),
        );
    }
    if documents && !cite {
        notes.push(
            "documents are sent without citations in this conversation: its earlier answers were made without \
             them, and turning them on now would change the messages those answers' thinking is bound to"
                .to_string(),
        );
    }
    // Answers of other models or APIs (this model's with other tools have the note above).
    let own = |t: &Turn| t.native.as_ref().is_some_and(|n| n.dialect == DIALECT && n.model == model);
    notes.extend(crate::provider::text_only_note(history, own));
    // Media of answers that go back as text cannot go along: an error (owner's rule).
    if let Some(why) = media_not_sent(as_text, ASSISTANT_TEXT_ONLY) {
        return Err(LlmError::request(why));
    }
    let warnings = Vec::new();
    Ok(Converted { system: (!system.is_empty()).then_some(Value::Array(system)), messages, notes, warnings, documents })
}

const ASSISTANT_TEXT_ONLY: &str =
    "Claude sees only the text of other models' answers; send it the file as an attachment";

pub use crate::provider::media_not_sent;

/// Documents may have citations on: no earlier answer that goes back natively
/// (its thinking is bound to the messages before it) was made without them
/// after a document (the native payload's `citations`; older answers have none).
pub fn citations_allowed(history: &[Turn], model: &str) -> bool {
    let mut documents = false;
    for t in history {
        match t.role {
            Role::User => documents |= t.parts.iter().any(is_document),
            Role::Assistant => {
                let native = t.native.as_ref().filter(|n| n.dialect == DIALECT && n.model == model);
                if documents
                    && let Some(n) = native
                    && n.payload.get("citations").and_then(Value::as_bool) != Some(true)
                {
                    return false;
                }
            }
            Role::System => {}
        }
    }
    true
}

/// A user part sent as a `document` block (PDFs and text files).
fn is_document(p: &Part) -> bool {
    match p {
        Part::File { blob, filename, .. } => {
            matches!(classify_mime(&blob.mime, filename.as_deref()), MediaClass::Pdf | MediaClass::Text)
        }
        _ => false,
    }
}

/// The message id (`msg_…`) of the latest answer of `model` on this API: the
/// previous request for the cache diagnostics (`null` without one).
pub fn previous_message_id(history: &[Turn], model: &str) -> Option<String> {
    history.iter().rev().find_map(|t| {
        let n = t.native.as_ref().filter(|n| t.role == Role::Assistant && n.dialect == DIALECT && n.model == model)?;
        n.payload.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).map(str::to_string)
    })
}

fn replay(turn: &Turn, model: &str, tools: Option<&Value>) -> Replay {
    let Some(native) = turn.native.as_ref().filter(|n| n.dialect == DIALECT && n.model == model) else {
        return Replay::Text;
    };
    let Some(content) = native.payload.get("content").and_then(Value::as_array).filter(|c| !c.is_empty()) else {
        return Replay::Text;
    };
    if tool_set(native.payload.get("tools")) != tool_set(tools) {
        return Replay::OtherTools;
    }
    // One assistant message per request of the turn, as they were sent.
    let hops: Vec<usize> = native
        .payload
        .get("hops")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).map(|n| n as usize).collect())
        .unwrap_or_default();
    if hops.len() > 1 && hops.iter().all(|&n| n > 0) && hops.iter().sum::<usize>() == content.len() {
        let mut out = Vec::new();
        let mut at = 0;
        for n in hops {
            out.push(json!({"role": "assistant", "content": &content[at..at + n]}));
            at += n;
        }
        return Replay::Native(out);
    }
    Replay::Native(vec![json!({"role": "assistant", "content": content})])
}

/// A request's tool list; no tools and an empty list are the same.
fn tool_set(tools: Option<&Value>) -> Option<&Value> {
    tools.filter(|t| t.as_array().is_some_and(|a| !a.is_empty()))
}

async fn user_content(turn: &Turn, model: &str, cite: bool, blobs: &dyn BlobLoader) -> Result<Vec<Value>, LlmError> {
    let mut out = Vec::new();
    for part in &turn.parts {
        match part {
            Part::Text { text, .. } => {
                if !text.is_empty() {
                    out.push(json!({"type": "text", "text": text}));
                }
            }
            Part::Image { blob, filename, .. } => {
                out.push(image(model, blob, filename.as_deref(), blobs).await?);
            }
            Part::File { blob, filename, .. } => {
                let name = filename.as_deref().filter(|n| !n.is_empty());
                match classify_mime(&blob.mime, name) {
                    MediaClass::Image => out.push(image(model, blob, name, blobs).await?),
                    MediaClass::Pdf => {
                        let bytes = load(blobs, blob).await?;
                        let source = json!({"type": "base64", "media_type": "application/pdf", "data": b64(&bytes)});
                        out.push(document(source, name, cite));
                    }
                    MediaClass::Text => {
                        let bytes = load(blobs, blob).await?;
                        let text = String::from_utf8(bytes).map_err(|_| {
                            LlmError::request(format!(
                                "{} is not UTF-8 text (`{model}` takes text files as plain-text documents)",
                                name.unwrap_or("the file")
                            ))
                        })?;
                        out.push(document(
                            json!({"type": "text", "media_type": "text/plain", "data": text}),
                            name,
                            cite,
                        ));
                    }
                    MediaClass::Audio | MediaClass::Video | MediaClass::Document | MediaClass::Other => {
                        return Err(LlmError::request(format!(
                            "`{model}` does not take {} files ({}): the Messages API takes text, images \
                             (JPEG, PNG, GIF, WebP), PDFs and plain-text documents",
                            blob.mime,
                            name.unwrap_or("file")
                        )));
                    }
                }
            }
            Part::RemoteMedia { url, .. } => {
                return Err(LlmError::request(format!("`{model}` cannot read remote media ({url}) via {NAME}")));
            }
            Part::Location { .. } => return Err(no_location(model, NAME)),
            Part::Reasoning(_) | Part::ToolCall(_) | Part::Refusal { .. } => {}
        }
    }
    Ok(out)
}

async fn image(
    model: &str,
    blob: &omni_core::BlobRef,
    name: Option<&str>,
    blobs: &dyn BlobLoader,
) -> Result<Value, LlmError> {
    let lower = blob.mime.to_ascii_lowercase();
    // `image/jpg` is a common mislabel of JPEG.
    let mime = if lower == "image/jpg" { "image/jpeg".to_string() } else { lower };
    if !IMAGE_TYPES.contains(&mime.as_str()) {
        return Err(LlmError::request(format!(
            "`{model}` does not take {} images{}: JPEG, PNG, GIF and WebP only",
            blob.mime,
            name.map(|n| format!(" ({n})")).unwrap_or_default()
        )));
    }
    let bytes = load(blobs, blob).await?;
    Ok(json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": b64(&bytes)}}))
}

fn document(source: Value, title: Option<&str>, cite: bool) -> Value {
    let mut d = json!({"type": "document", "source": source});
    if let Some(t) = title {
        d["title"] = json!(t);
    }
    if cite {
        d["citations"] = json!({"enabled": true});
    }
    d
}

/// Put a cache breakpoint on the last block of the user message before the
/// newest one: the end of the previous request's prompt, where that request's
/// automatic breakpoint wrote its cache entry. A breakpoint there finds the
/// entry even when the answer in between added more than the 20 blocks the
/// API looks back (web search answers often do). Cache markers are not part
/// of the preserved-thinking prefix. Returns where the marker went.
pub fn mark_previous_user_turn(messages: &mut [Value]) -> Option<String> {
    let users: Vec<usize> = messages.iter().enumerate().filter(|(_, m)| m["role"] == "user").map(|(i, _)| i).collect();
    let &i = users.iter().rev().nth(1)?;
    let content = messages[i].get_mut("content")?.as_array_mut()?;
    let j = content.len().checked_sub(1)?;
    content[j].as_object_mut()?.insert("cache_control".into(), json!({"type": "ephemeral"}));
    Some(format!("messages[{i}].content[{j}]"))
}
