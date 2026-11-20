//! Conversation → `contents` and `systemInstruction`.
//!
//! - System turns: text parts of `systemInstruction` ("currently, text only"),
//!   in order (the API has one system field, like Anthropic's).
//! - Assistant turns produced by this dialect: the stored model parts,
//!   verbatim (thought summaries, thought signatures, code execution,
//!   generated images, exactly as received), also when the conversation
//!   continues on another Gemini model (`/m`): Google's only guidance on
//!   switching models is to "still resend the previous model's thought blocks.
//!   The backend manages compatibility" (Interactions API; none for
//!   generateContent: docs/research/gemini.md §3.3). Any other assistant turn:
//!   its answer text, and the images and files it generated as `inlineData`
//!   parts where they were (model turns take any part; thought signatures are
//!   required only on function calls, so none is made up), at the same media
//!   resolution as user media; a kind of media the model does not take is
//!   left out with a warning. Drafts made while thinking are reasoning and
//!   stay out.
//! - User parts: text; images, PDFs, Office/OpenDocument files, audio and
//!   video as `inlineData` (with the file name as `displayName`); text files
//!   natively as text `inlineData` (`text/plain` unless their type is one
//!   Google lists); YouTube links as `fileData.fileUri`. Media resolution:
//!   the highest per-part level on Gemini 3/4 (images `ULTRA_HIGH`, PDFs and
//!   video `HIGH`). A Telegram location is returned for
//!   `toolConfig.retrievalConfig` (Google Maps grounding) and also goes as a
//!   text part where it was ([`location_text`]): the model never sees
//!   `latLng`, only the Maps tool does, and Google's own country hint (where
//!   the bot's server is) often won over it (2026-11-20; owner's choice A).

use omni_core::content::{Part, RemoteMediaKind, Role, Turn};
use serde_json::{Value, json};

use super::models::ModelSpec;
use crate::error::LlmError;
use crate::provider::{b64, load};
use crate::{BlobLoader, MediaClass, classify_mime};

/// Text types the API lists for inline data; other text files go as `text/plain`.
const TEXT_MIMES: &[&str] = &[
    "text/plain",
    "text/html",
    "text/css",
    "text/javascript",
    "text/x-typescript",
    "text/csv",
    "text/markdown",
    "text/x-python",
    "text/xml",
    "text/rtf",
    "application/json",
    "application/x-ipynb+json",
    "application/x-python-code",
    "application/x-javascript",
    "application/x-typescript",
    "application/rtf",
];

/// Office and OpenDocument files the API takes (document processing guide).
const OFFICE: &[(&str, &str)] = &[
    ("docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
    ("doc", "application/msword"),
    ("dotx", "application/vnd.openxmlformats-officedocument.wordprocessingml.template"),
    ("odt", "application/vnd.oasis.opendocument.text"),
    ("pptx", "application/vnd.openxmlformats-officedocument.presentationml.presentation"),
    ("ppt", "application/vnd.ms-powerpoint"),
    ("odp", "application/vnd.oasis.opendocument.presentation"),
    ("xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
    ("xls", "application/vnd.ms-excel"),
    ("ods", "application/vnd.oasis.opendocument.spreadsheet"),
];

/// A Telegram location as the model reads it: `[Location sent by the user:
/// <venue>, <address>; latitude …, longitude …, within about N m]` (six
/// decimals: about 0.1 m).
pub fn location_text(
    lat: f64,
    lng: f64,
    accuracy_m: Option<u32>,
    title: Option<&str>,
    address: Option<&str>,
) -> String {
    let place: Vec<&str> = [title, address].into_iter().flatten().filter(|s| !s.trim().is_empty()).collect();
    let mut s = String::from("[Location sent by the user: ");
    if !place.is_empty() {
        s.push_str(&place.join(", "));
        s.push_str("; ");
    }
    s.push_str(&format!("latitude {lat:.6}, longitude {lng:.6}"));
    if let Some(a) = accuracy_m {
        s.push_str(&format!(", within about {a} m"));
    }
    s.push(']');
    s
}

pub const RES_ULTRA_HIGH: &str = "MEDIA_RESOLUTION_ULTRA_HIGH";
pub const RES_HIGH: &str = "MEDIA_RESOLUTION_HIGH";

/// The Office/OpenDocument MIME type of a file, by type or extension.
fn office_mime(mime: &str, filename: Option<&str>) -> Option<&'static str> {
    let m = mime.to_ascii_lowercase();
    if let Some((_, t)) = OFFICE.iter().find(|(_, t)| *t == m) {
        return Some(t);
    }
    let ext = filename.and_then(|f| f.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase())?;
    OFFICE.iter().find(|(e, _)| *e == ext).map(|(_, t)| *t)
}

/// Media sent with the request (for the settings / applied record).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaSeen {
    pub image: bool,
    pub pdf: bool,
    pub video: bool,
    pub audio: bool,
    pub office: bool,
    pub text_file: bool,
}

impl MediaSeen {
    /// Media other than text (files).
    pub fn multimodal(&self) -> bool {
        self.image || self.pdf || self.video || self.audio || self.office
    }
}

#[derive(Debug, Clone, Default)]
pub struct Converted {
    pub contents: Vec<Value>,
    pub system: Option<Value>,
    /// The latest location in the conversation (latitude, longitude).
    pub location: Option<(f64, f64)>,
    pub media: MediaSeen,
    /// What could not be sent (generated media of a kind the model does not take).
    pub warnings: Vec<String>,
}

/// Model turns of `contents` hold the built-in tools' own calls or results
/// (`toolCall` / `toolResponse` parts, returned with
/// `toolConfig.includeServerSideToolInvocations`).
pub fn has_tool_invocations(contents: &[Value]) -> bool {
    contents.iter().filter(|c| c.get("role").and_then(Value::as_str) == Some("model")).any(|c| {
        c.get("parts")
            .and_then(Value::as_array)
            .is_some_and(|ps| ps.iter().any(|p| p.get("toolCall").is_some() || p.get("toolResponse").is_some()))
    })
}

/// The media class of a file as the API takes it (Office files by type or extension).
fn file_class(mime: &str, filename: Option<&str>) -> (MediaClass, Option<&'static str>) {
    match office_mime(mime, filename) {
        Some(m) => (MediaClass::Document, Some(m)),
        None => (classify_mime(mime, filename), None),
    }
}

fn inline(mime: &str, bytes: &[u8], name: Option<&str>) -> Value {
    let mut d = json!({"mimeType": mime, "data": b64(bytes)});
    if let Some(n) = name {
        d["displayName"] = json!(n);
    }
    json!({ "inlineData": d })
}

fn with_resolution(mut part: Value, level: Option<&str>) -> Value {
    if let Some(l) = level {
        part["mediaResolution"] = json!({ "level": l });
    }
    part
}

fn unsupported(model: &str, what: &str, name: &str) -> LlmError {
    LlmError::request(format!("`{model}` does not take {what} input on the Gemini API ({name})"))
}

/// Convert the conversation for `model`.
pub async fn convert(
    history: &[Turn],
    model: &str,
    spec: &ModelSpec,
    blobs: &dyn BlobLoader,
) -> Result<Converted, LlmError> {
    let mut out = Converted::default();
    let mut system: Vec<Value> = Vec::new();
    let res = |level: &'static str| spec.per_part_media_resolution.then_some(level);
    let last_user = history.iter().rposition(|t| t.role == Role::User);
    // Generated media of other answers the model does not take: (an image part, its kind).
    let mut left_out: Vec<(bool, String)> = Vec::new();
    for (idx, turn) in history.iter().enumerate() {
        match turn.role {
            Role::System => {
                if turn.parts.iter().any(|p| !matches!(p, Part::Text { .. })) {
                    return Err(LlmError::request(
                        "the Gemini API's system instruction takes text only; this system turn has other content",
                    ));
                }
                let text = turn.answer_text();
                if !text.is_empty() {
                    system.push(json!({ "text": text }));
                }
            }
            Role::Assistant => {
                let parts = turn
                    .native
                    .as_ref()
                    .filter(|n| n.dialect == super::DIALECT)
                    .and_then(|n| n.payload.get("parts")?.as_array())
                    .filter(|p| !p.is_empty());
                if let Some(parts) = parts {
                    out.contents.push(json!({"role": "model", "parts": parts}));
                    continue;
                }
                // Another API's answer: its text, with what it generated where it was.
                let mut parts: Vec<Value> = Vec::new();
                let mut text = String::new();
                let flush = |text: &mut String, parts: &mut Vec<Value>| {
                    if !text.is_empty() {
                        parts.push(json!({ "text": std::mem::take(text) }));
                    }
                };
                for part in &turn.parts {
                    match part {
                        Part::Text { text: t, .. } => text.push_str(t),
                        Part::Image { thought: true, .. } => {}
                        Part::Image { blob, filename, .. } => {
                            if !spec.inputs.image {
                                left_out.push((true, "image".into()));
                                continue;
                            }
                            flush(&mut text, &mut parts);
                            let bytes = load(blobs, blob).await?;
                            out.media.image = true;
                            parts.push(with_resolution(
                                inline(&blob.mime, &bytes, filename.as_deref()),
                                res(RES_ULTRA_HIGH),
                            ));
                        }
                        Part::File { blob, filename, .. } => {
                            let fname = filename.as_deref();
                            let (class, office) = file_class(&blob.mime, fname);
                            let (taken, kind, level, mime) = match class {
                                MediaClass::Image => (spec.inputs.image, "image", res(RES_ULTRA_HIGH), None),
                                MediaClass::Pdf => (spec.inputs.pdf, "PDF", res(RES_HIGH), Some("application/pdf")),
                                MediaClass::Video => (spec.inputs.video, "video", res(RES_HIGH), None),
                                MediaClass::Audio => (spec.inputs.audio, "audio", None, None),
                                MediaClass::Document => (spec.inputs.office, "Office/OpenDocument", None, office),
                                MediaClass::Text => (true, "text", None, None),
                                MediaClass::Other => (false, blob.mime.as_str(), None, None),
                            };
                            if !taken {
                                left_out.push((false, kind.to_string()));
                                continue;
                            }
                            let bytes = load(blobs, blob).await?;
                            let mime = match class {
                                MediaClass::Text => {
                                    let m = blob.mime.to_ascii_lowercase();
                                    TEXT_MIMES.iter().find(|t| **t == m).copied().unwrap_or("text/plain").to_string()
                                }
                                _ => mime.map(str::to_string).unwrap_or_else(|| blob.mime.clone()),
                            };
                            match class {
                                MediaClass::Image => out.media.image = true,
                                MediaClass::Pdf => out.media.pdf = true,
                                MediaClass::Video => out.media.video = true,
                                MediaClass::Audio => out.media.audio = true,
                                MediaClass::Document => out.media.office = true,
                                _ => out.media.text_file = true,
                            }
                            flush(&mut text, &mut parts);
                            parts.push(with_resolution(inline(&mime, &bytes, fname), level));
                        }
                        Part::RemoteMedia { .. }
                        | Part::Location { .. }
                        | Part::Reasoning(_)
                        | Part::ToolCall(_)
                        | Part::Refusal { .. } => {}
                    }
                }
                flush(&mut text, &mut parts);
                if !parts.is_empty() {
                    out.contents.push(json!({"role": "model", "parts": parts}));
                }
            }
            Role::User => {
                let mut parts: Vec<Value> = Vec::new();
                for part in &turn.parts {
                    match part {
                        Part::Text { text, .. } => {
                            if !text.is_empty() {
                                parts.push(json!({ "text": text }));
                            }
                        }
                        Part::Image { blob, filename, .. } => {
                            if !spec.inputs.image {
                                return Err(unsupported(model, "image", filename.as_deref().unwrap_or("image")));
                            }
                            let bytes = load(blobs, blob).await?;
                            out.media.image = true;
                            parts.push(with_resolution(
                                inline(&blob.mime, &bytes, filename.as_deref()),
                                res(RES_ULTRA_HIGH),
                            ));
                        }
                        Part::File { blob, filename, .. } => {
                            let name = filename.as_deref().unwrap_or("file");
                            let (class, office) = file_class(&blob.mime, filename.as_deref());
                            let fname = filename.as_deref();
                            match class {
                                MediaClass::Image => {
                                    if !spec.inputs.image {
                                        return Err(unsupported(model, "image", name));
                                    }
                                    let bytes = load(blobs, blob).await?;
                                    out.media.image = true;
                                    parts.push(with_resolution(inline(&blob.mime, &bytes, fname), res(RES_ULTRA_HIGH)));
                                }
                                MediaClass::Pdf => {
                                    if !spec.inputs.pdf {
                                        return Err(unsupported(model, "PDF", name));
                                    }
                                    let bytes = load(blobs, blob).await?;
                                    out.media.pdf = true;
                                    parts
                                        .push(with_resolution(inline("application/pdf", &bytes, fname), res(RES_HIGH)));
                                }
                                MediaClass::Document => {
                                    let Some(mime) = office else {
                                        return Err(LlmError::request(format!(
                                            "unsupported document type {} ({name}); the Gemini API takes PDF, \
                                             Office (docx, doc, dotx, pptx, ppt, xlsx, xls) and OpenDocument \
                                             (odt, odp, ods) files",
                                            blob.mime
                                        )));
                                    };
                                    if !spec.inputs.office {
                                        return Err(unsupported(model, "Office/OpenDocument", name));
                                    }
                                    let bytes = load(blobs, blob).await?;
                                    out.media.office = true;
                                    parts.push(inline(mime, &bytes, fname));
                                }
                                MediaClass::Audio => {
                                    if !spec.inputs.audio {
                                        return Err(unsupported(model, "audio", name));
                                    }
                                    let bytes = load(blobs, blob).await?;
                                    out.media.audio = true;
                                    parts.push(inline(&blob.mime, &bytes, fname));
                                }
                                MediaClass::Video => {
                                    if !spec.inputs.video {
                                        return Err(unsupported(model, "video", name));
                                    }
                                    let bytes = load(blobs, blob).await?;
                                    out.media.video = true;
                                    parts.push(with_resolution(inline(&blob.mime, &bytes, fname), res(RES_HIGH)));
                                }
                                MediaClass::Text => {
                                    let bytes = load(blobs, blob).await?;
                                    if std::str::from_utf8(&bytes).is_err() {
                                        return Err(LlmError::request(format!("{name} is not valid UTF-8 text")));
                                    }
                                    let m = blob.mime.to_ascii_lowercase();
                                    let mime = TEXT_MIMES.iter().find(|t| **t == m).copied().unwrap_or("text/plain");
                                    out.media.text_file = true;
                                    parts.push(inline(mime, &bytes, fname));
                                }
                                MediaClass::Other => {
                                    return Err(LlmError::request(format!(
                                        "unsupported file type {} ({name})",
                                        blob.mime
                                    )));
                                }
                            }
                        }
                        Part::RemoteMedia { url, kind } => {
                            if *kind != RemoteMediaKind::YouTube {
                                return Err(LlmError::request(format!(
                                    "only YouTube links are sent to the Gemini API as remote media ({url})"
                                )));
                            }
                            if !spec.inputs.video {
                                return Err(unsupported(model, "video (YouTube)", url));
                            }
                            out.media.video = true;
                            parts.push(with_resolution(json!({"fileData": {"fileUri": url}}), res(RES_HIGH)));
                        }
                        Part::Location { latitude, longitude, accuracy_m, title, address } => {
                            // Latest wins: the user's current position for Google Maps grounding.
                            out.location = Some((*latitude, *longitude));
                            let text =
                                location_text(*latitude, *longitude, *accuracy_m, title.as_deref(), address.as_deref());
                            parts.push(json!({ "text": text }));
                        }
                        // Never in user turns.
                        Part::Reasoning(_) | Part::ToolCall(_) | Part::Refusal { .. } => {}
                    }
                }
                let only_location = turn.location().is_some()
                    && turn.parts.iter().all(|p| match p {
                        Part::Location { .. } => true,
                        Part::Text { text, .. } => text.trim().is_empty(),
                        _ => false,
                    });
                if only_location && Some(idx) == last_user {
                    return Err(LlmError::request(
                        "the message has nothing to send to the Gemini API besides a location \
                         (your position for Google Maps grounding, `+m`): add a question",
                    ));
                }
                if parts.is_empty() {
                    continue;
                }
                out.contents.push(json!({"role": "user", "parts": parts}));
            }
        }
    }
    if !system.is_empty() {
        out.system = Some(json!({ "parts": system }));
    }
    if !left_out.is_empty() {
        let images = left_out.iter().filter(|(image, _)| *image).count();
        let files = left_out.len() - images;
        let count = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut what = Vec::new();
        if images > 0 {
            what.push(count(images, "image", "images"));
        }
        if files > 0 {
            what.push(count(files, "file", "files"));
        }
        let mut kinds: Vec<&str> = left_out.iter().map(|(_, k)| k.as_str()).collect();
        kinds.sort_unstable();
        kinds.dedup();
        // Content the model cannot take refuses the request (owner's rule).
        return Err(LlmError::request(format!(
            "{} generated in earlier turns cannot be sent: `{model}` does not take {} input",
            what.join(" and "),
            kinds.join(", ")
        )));
    }
    Ok(out)
}
