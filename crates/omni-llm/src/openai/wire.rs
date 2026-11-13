//! The Responses API wire format of the platforms' own APIs: OpenAI's
//! `POST /v1/responses` and xAI's, which follows it. Shared by [`super`]
//! (OpenAI) and [`crate::xai`], each with its own dialect, model table,
//! options and error classification.
//!
//! - History: earlier turns of the same dialect and model replay their output
//!   items verbatim (encrypted reasoning, messages with `phase`, hosted tool
//!   calls); other assistant turns become an assistant message with their
//!   text. Their media (an image another model generated, a file a code run
//!   wrote) go along only where the API takes them in an assistant message:
//!   xAI's message content takes images and files whatever the role
//!   (`openapi.json`); OpenAI shows the model only the text of other models' answers; send it the file as an attachment (its own
//!   `image_generation_call` items replay only with the model's own turns).
//!   What is not sent is said in a warning. User parts become `input_text` /
//!   `input_image` / `input_file` per what each API takes.
//! - Stream: OpenRouter's Responses parser ([`responses::EventParser`], the
//!   same OpenAI event format) wrapped with what the direct APIs add: search
//!   sources and results and code outputs on tool calls, usage normalized
//!   across the two APIs with the cost (reported by xAI, estimated for
//!   OpenAI), the platform's own in-band error classification, generated
//!   images of code runs, the files they wrote in their container (OpenAI,
//!   fetched where cited: [`super::files`]), notices for what the response
//!   reports about its input (OpenAI `input_transformations` and prompt cache
//!   misses due to a changed conversation or tools; xAI documents searched
//!   with `attachment_search`), xAI's exact context size
//!   (`usage.context_details`), and the `image_generation` tool (`+i`): each
//!   call's progress and prompt as a tool call, its partial images as
//!   previews (OpenAI) and its final image where the call finished.
//! - A broken OpenAI stream is recovered from the stored copy of its
//!   response ([`super::recover`]): the whole output again, as the stream
//!   would have given it.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use omni_core::content::{Citation, Part, Role, ToolCall, Turn};
use omni_core::usage::CostSource;
use omni_core::{BlobRef, Usage};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::files::{Fetch, Fetcher, Gate, Queued};
use super::recover;
use crate::error::{ErrorKind, LlmError, Retry};
use crate::event::StreamEvent;
use crate::openrouter::chat::{ErrorMapper, decode_data_url};
use crate::openrouter::responses;
use crate::{BlobLoader, CallOutcome, EventSink, MediaClass, Observer, Prepared, classify_mime, provider};

/// The two official Responses APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    OpenAi,
    Xai,
}

impl Platform {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Platform::OpenAi => "OpenAI",
            Platform::Xai => "xAI",
        }
    }

    pub(crate) fn dialect(self) -> &'static str {
        match self {
            Platform::OpenAi => super::DIALECT,
            Platform::Xai => crate::xai::DIALECT,
        }
    }
}

// ------------------------------------------------------------------ history

/// What the request's model takes.
pub(crate) struct Inputs<'a> {
    pub platform: Platform,
    pub model: &'a str,
    /// `detail` of every `input_image` (the highest the model documents).
    pub image_detail: &'static str,
}

/// The converted conversation.
#[derive(Debug, Default)]
pub(crate) struct Converted {
    pub input: Vec<Value>,
    /// Shown with the answer (e.g. reasoning OpenAI withheld).
    pub notes: Vec<String>,
    /// Media of earlier answers the API cannot take (see [`Unsent`]).
    pub warnings: Vec<String>,
    pub images: usize,
    pub pdfs: usize,
    pub files: usize,
    /// The response id of the latest turn replayed natively (OpenAI's prompt
    /// cache diagnostics compare the request with it).
    pub last_response_id: Option<String>,
}

/// The conversation as Responses `input` items.
pub(crate) async fn convert_history(
    history: &[Turn],
    inp: &Inputs<'_>,
    blobs: &dyn BlobLoader,
) -> Result<Converted, LlmError> {
    let mut c = Converted::default();
    let mut unsent = Unsent::default();
    for turn in history {
        match turn.role {
            Role::System => c.input.push(json!({
                "type": "message",
                "role": "system",
                "content": [{"type": "input_text", "text": turn.answer_text()}],
            })),
            Role::Assistant => {
                // The exact output items, when this API and model produced them.
                if let Some(n) = &turn.native
                    && n.dialect == inp.platform.dialect()
                    && n.model == inp.model
                    && let Some(items) = n.payload.get("output").and_then(Value::as_array)
                {
                    c.input.extend(items.iter().cloned());
                    if let Some(id) = n.payload.get("id").and_then(Value::as_str) {
                        c.last_response_id = Some(id.to_string());
                    }
                    continue;
                }
                assistant_turn(turn, inp, blobs, &mut c, &mut unsent).await?;
            }
            Role::User => {
                let mut content = Vec::new();
                for part in &turn.parts {
                    user_part(part, inp, blobs, &mut content, &mut c).await?;
                }
                c.input.push(json!({"type": "message", "role": "user", "content": content}));
            }
        }
    }
    // Media of earlier answers the API cannot take refuse the request (owner's rule:
    // content a model cannot take is an error, never dropped).
    if let Some(why) = unsent.message() {
        return Err(LlmError::request(why));
    }
    Ok(c)
}

/// What media of earlier answers cannot be sent, by reason.
#[derive(Debug, Default)]
struct Unsent(Vec<(&'static str, [usize; 4])>);

impl Unsent {
    /// One medium (`kind`: image, video, audio, other file) that cannot be sent, and why.
    fn add(&mut self, kind: usize, why: &'static str) {
        match self.0.iter_mut().find(|(w, _)| *w == why) {
            Some((_, n)) => n[kind] += 1,
            None => {
                let mut n = [0; 4];
                n[kind] = 1;
                self.0.push((why, n));
            }
        }
    }

    /// The error message, when anything cannot be sent.
    fn message(&self) -> Option<String> {
        const NAMES: [(&str, &str); 4] =
            [("image", "images"), ("video", "videos"), ("audio file", "audio files"), ("file", "files")];
        self.0
            .iter()
            .map(|(why, n)| {
                let what: Vec<String> = n
                    .iter()
                    .zip(NAMES)
                    .filter(|(k, _)| **k > 0)
                    .map(|(k, (one, many))| format!("{k} {}", if *k == 1 { one } else { many }))
                    .collect();
                let list = match what.split_last() {
                    Some((last, [])) => last.clone(),
                    Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
                    None => String::new(),
                };
                format!("{list} from earlier answers cannot be sent: {why}")
            })
            .reduce(|a, b| format!("{a}; {b}"))
    }
}

const IMAGE: usize = 0;
const VIDEO: usize = 1;
const AUDIO: usize = 2;
const FILE: usize = 3;

/// An answer this API and model did not produce (another model or
/// platform): its text, and its media where the API takes them in an
/// assistant message, in their order. Drafts made while thinking are
/// reasoning, not part of the answer.
async fn assistant_turn(
    turn: &Turn,
    inp: &Inputs<'_>,
    blobs: &dyn BlobLoader,
    c: &mut Converted,
    unsent: &mut Unsent,
) -> Result<(), LlmError> {
    let kind = |p: &Part| match p {
        Part::Image { thought: false, .. } => Some(IMAGE),
        Part::File { blob, filename, .. } => Some(match classify_mime(&blob.mime, filename.as_deref()) {
            MediaClass::Image => IMAGE,
            MediaClass::Video => VIDEO,
            MediaClass::Audio => AUDIO,
            _ => FILE,
        }),
        _ => None,
    };
    let plain = json!({"type": "message", "role": "assistant", "content": turn.answer_text()});
    if inp.platform == Platform::OpenAi {
        // Assistant content is `output_text` or `refusal` only.
        for k in turn.parts.iter().filter_map(kind) {
            unsent.add(
                k,
                "OpenAI shows the model only the text of other models' answers; send it the file as an attachment",
            );
        }
        c.input.push(plain);
        return Ok(());
    }
    // xAI: a message's content takes text, images and files, whatever its role.
    let mut content = Vec::new();
    let mut media = 0;
    for part in &turn.parts {
        match (part, kind(part)) {
            (Part::Text { text, .. }, _) if !text.is_empty() => {
                content.push(json!({"type": "output_text", "text": text}));
            }
            (Part::Image { blob, filename, .. } | Part::File { blob, filename, .. }, Some(IMAGE)) => {
                let mime = base_mime(&blob.mime);
                if !matches!(mime.as_str(), "image/png" | "image/jpeg" | "image/jpg") || blob.size > XAI_IMAGE_MAX {
                    unsent.add(IMAGE, "xAI takes JPEG or PNG images of up to 20 MiB");
                    continue;
                }
                content.push(image(blob, filename.as_deref(), inp, blobs).await?);
                c.images += 1;
                media += 1;
            }
            (Part::File { blob, filename, .. }, Some(k)) => {
                let class = classify_mime(&blob.mime, filename.as_deref());
                match (k, class) {
                    (_, MediaClass::Pdf | MediaClass::Text) => {
                        let name = filename.clone().unwrap_or_else(|| default_name(&blob.mime));
                        xai_file(blob, &name, class, inp, blobs, &mut content, c).await?;
                        media += 1;
                    }
                    (VIDEO, _) => unsent.add(VIDEO, "xAI takes no video input"),
                    (AUDIO, _) => unsent.add(AUDIO, "xAI takes no audio input"),
                    _ => unsent.add(FILE, "xAI reads PDF and text-based files only"),
                }
            }
            _ => {}
        }
    }
    c.input.push(if media > 0 { json!({"type": "message", "role": "assistant", "content": content}) } else { plain });
    Ok(())
}

async fn user_part(
    part: &Part,
    inp: &Inputs<'_>,
    blobs: &dyn BlobLoader,
    content: &mut Vec<Value>,
    c: &mut Converted,
) -> Result<(), LlmError> {
    let (model, via) = (inp.model, inp.platform.name());
    match part {
        Part::Text { text, .. } => {
            if !text.is_empty() {
                content.push(json!({"type": "input_text", "text": text}));
            }
        }
        Part::Image { blob, filename, .. } => {
            content.push(image(blob, filename.as_deref(), inp, blobs).await?);
            c.images += 1;
        }
        Part::File { blob, filename, .. } => {
            let name = filename.clone().unwrap_or_else(|| default_name(&blob.mime));
            match classify_mime(&blob.mime, filename.as_deref()) {
                MediaClass::Image => {
                    content.push(image(blob, Some(&name), inp, blobs).await?);
                    c.images += 1;
                }
                MediaClass::Audio => {
                    return Err(LlmError::request(format!(
                        "`{model}` does not take audio via {via} ({name}, {}): the Responses API has no audio input",
                        blob.mime
                    )));
                }
                MediaClass::Video => {
                    return Err(LlmError::request(format!(
                        "`{model}` does not take video via {via} ({name}, {}): the Responses API has no video input",
                        blob.mime
                    )));
                }
                class => match inp.platform {
                    Platform::OpenAi => {
                        openai_file(blob, &name, filename.as_deref(), class, inp, blobs, content, c).await?
                    }
                    Platform::Xai => xai_file(blob, &name, class, inp, blobs, content, c).await?,
                },
            }
        }
        Part::RemoteMedia { url, .. } => {
            return Err(LlmError::request(format!("`{model}` cannot read remote media via {via} ({url})")));
        }
        Part::Location { .. } => return Err(provider::no_location(model, via)),
        // Never in user turns.
        Part::Reasoning(_) | Part::ToolCall(_) | Part::Refusal { .. } => {}
    }
    Ok(())
}

/// `image/png` from `image/PNG; charset=…`.
fn base_mime(mime: &str) -> String {
    mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

/// A name for a file Telegram gave none (xAI requires one with inline data).
fn default_name(mime: &str) -> String {
    match base_mime(mime).as_str() {
        "application/pdf" => "file.pdf".into(),
        "text/plain" => "file.txt".into(),
        _ => "file".into(),
    }
}

/// xAI: images up to 20 MiB (docs.x.ai "Models", image input models).
const XAI_IMAGE_MAX: u64 = 20 * 1024 * 1024;

async fn image(
    blob: &BlobRef,
    name: Option<&str>,
    inp: &Inputs<'_>,
    blobs: &dyn BlobLoader,
) -> Result<Value, LlmError> {
    let mime = base_mime(&blob.mime);
    let what = name.map(|n| format!("{n}, {}", blob.mime)).unwrap_or_else(|| blob.mime.clone());
    match inp.platform {
        // OpenAI images and vision guide: PNG, JPEG, WEBP, non-animated GIF.
        Platform::OpenAi
            if !matches!(mime.as_str(), "image/png" | "image/jpeg" | "image/jpg" | "image/webp" | "image/gif") =>
        {
            return Err(LlmError::request(format!(
                "`{}` does not take this image via OpenAI ({what}): PNG, JPEG, WEBP or non-animated GIF only",
                inp.model
            )));
        }
        Platform::Xai if !matches!(mime.as_str(), "image/png" | "image/jpeg" | "image/jpg") => {
            return Err(LlmError::request(format!(
                "`{}` does not take this image via xAI ({what}): JPEG or PNG only",
                inp.model
            )));
        }
        Platform::Xai if blob.size > XAI_IMAGE_MAX => {
            return Err(LlmError::request(format!(
                "`{}` does not take images over 20 MiB via xAI ({what}, {} bytes)",
                inp.model, blob.size
            )));
        }
        _ => {}
    }
    let bytes = provider::load(blobs, blob).await?;
    Ok(json!({"type": "input_image", "image_url": provider::data_url(&blob.mime, &bytes), "detail": inp.image_detail}))
}

/// MIME types OpenAI's `input_file` takes (file inputs guide, "Full list of
/// accepted file types", fetched 2026-11-12). Compared in lowercase.
const OPENAI_FILE_MIMES: &[&str] = &[
    "application/pdf",
    // spreadsheets
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "application/vnd.ms-excel",
    "text/csv",
    "application/csv",
    "text/tsv",
    "text/x-iif",
    "application/x-iif",
    "application/vnd.google-apps.spreadsheet",
    // rich documents
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/msword",
    "application/rtf",
    "text/rtf",
    "application/vnd.oasis.opendocument.text",
    "application/vnd.apple.pages",
    "application/vnd.google-apps.document",
    "application/vnd.apple.iwork",
    // presentations
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "application/vnd.ms-powerpoint",
    "application/vnd.apple.keynote",
    "application/vnd.google-apps.presentation",
    // text and code
    "application/javascript",
    "application/typescript",
    "text/xml",
    "text/x-shellscript",
    "text/x-rst",
    "text/x-makefile",
    "text/x-lisp",
    "text/x-asm",
    "text/vbscript",
    "text/css",
    "message/rfc822",
    "application/x-sql",
    "application/x-scala",
    "application/x-rust",
    "application/x-powershell",
    "text/x-diff",
    "text/x-patch",
    "application/x-patch",
    "text/plain",
    "text/markdown",
    "text/x-java",
    "text/x-script.python",
    "text/x-python",
    "text/x-c",
    "text/x-c++",
    "text/x-golang",
    "text/html",
    "text/x-php",
    "application/x-php",
    "application/x-httpd-php",
    "application/x-httpd-php-source",
    "text/x-ruby",
    "text/x-sh",
    "text/x-bash",
    "application/x-bash",
    "text/x-zsh",
    "text/x-tex",
    "text/x-csharp",
    "application/json",
    "text/x-typescript",
    "text/javascript",
    "text/x-go",
    "text/x-rust",
    "text/x-scala",
    "text/x-kotlin",
    "text/x-swift",
    "text/x-lua",
    "text/x-r",
    "text/x-julia",
    "text/x-perl",
    "text/x-objectivec",
    "text/x-objectivec++",
    "text/x-erlang",
    "text/x-elixir",
    "text/x-haskell",
    "text/x-clojure",
    "text/x-groovy",
    "text/x-dart",
    "text/x-awk",
    "application/x-awk",
    "text/jsx",
    "text/tsx",
    "text/x-handlebars",
    "text/x-mustache",
    "text/x-ejs",
    "text/x-jinja2",
    "text/x-liquid",
    "text/x-erb",
    "text/x-twig",
    "text/x-pug",
    "text/x-jade",
    "text/x-tmpl",
    "text/x-cmake",
    "text/x-dockerfile",
    "text/x-gradle",
    "text/x-ini",
    "text/x-properties",
    "text/x-protobuf",
    "application/x-protobuf",
    "text/x-sql",
    "text/x-sass",
    "text/x-scss",
    "text/x-less",
    "text/x-hcl",
    "text/x-terraform",
    "application/x-terraform",
    "text/x-toml",
    "application/x-toml",
    "application/graphql",
    "application/x-graphql",
    "text/x-graphql",
    "application/x-ndjson",
    "application/json5",
    "application/x-json5",
    "text/x-yaml",
    "application/toml",
    "application/x-yaml",
    "application/yaml",
    "text/x-astro",
    "text/srt",
    "application/x-subrip",
    "text/x-subrip",
    "text/vtt",
    "text/x-vcard",
    "text/calendar",
];

/// File name extensions → an accepted MIME type, for files Telegram labels
/// with a generic type (`application/octet-stream`): the guide's extension
/// list, then common source-code extensions whose MIME type is on its list.
const OPENAI_FILE_EXTS: &[(&str, &str)] = &[
    ("pdf", "application/pdf"),
    ("xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
    ("xls", "application/vnd.ms-excel"),
    ("xla", "application/vnd.ms-excel"),
    ("xlb", "application/vnd.ms-excel"),
    ("xlc", "application/vnd.ms-excel"),
    ("xlm", "application/vnd.ms-excel"),
    ("xlt", "application/vnd.ms-excel"),
    ("xlw", "application/vnd.ms-excel"),
    ("csv", "text/csv"),
    ("tsv", "text/tsv"),
    ("iif", "text/x-iif"),
    ("docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
    ("doc", "application/msword"),
    ("dot", "application/msword"),
    ("odt", "application/vnd.oasis.opendocument.text"),
    ("rtf", "application/rtf"),
    ("pages", "application/vnd.apple.pages"),
    ("pptx", "application/vnd.openxmlformats-officedocument.presentationml.presentation"),
    ("ppt", "application/vnd.ms-powerpoint"),
    ("pot", "application/vnd.ms-powerpoint"),
    ("ppa", "application/vnd.ms-powerpoint"),
    ("pps", "application/vnd.ms-powerpoint"),
    ("pwz", "application/vnd.ms-powerpoint"),
    ("wiz", "application/vnd.ms-powerpoint"),
    ("key", "application/vnd.apple.keynote"),
    ("asm", "text/x-asm"),
    ("s", "text/x-asm"),
    ("bat", "text/plain"),
    ("c", "text/x-c"),
    ("h", "text/x-c"),
    ("cc", "text/x-c++"),
    ("cpp", "text/x-c++"),
    ("cxx", "text/x-c++"),
    ("hh", "text/x-c++"),
    ("conf", "text/plain"),
    ("def", "text/plain"),
    ("dic", "text/plain"),
    ("in", "text/plain"),
    ("list", "text/plain"),
    ("log", "text/plain"),
    ("text", "text/plain"),
    ("txt", "text/plain"),
    ("css", "text/css"),
    ("eml", "message/rfc822"),
    ("mht", "message/rfc822"),
    ("mhtml", "message/rfc822"),
    ("mime", "message/rfc822"),
    ("nws", "message/rfc822"),
    ("htm", "text/html"),
    ("html", "text/html"),
    ("ics", "text/calendar"),
    ("ifb", "text/calendar"),
    ("js", "text/javascript"),
    ("mjs", "text/javascript"),
    ("json", "application/json"),
    ("ksh", "text/x-shellscript"),
    ("markdown", "text/markdown"),
    ("md", "text/markdown"),
    ("pl", "text/x-perl"),
    ("py", "text/x-python"),
    ("rst", "text/x-rst"),
    ("sql", "application/x-sql"),
    ("srt", "text/srt"),
    ("vcf", "text/x-vcard"),
    ("vtt", "text/vtt"),
    ("xml", "text/xml"),
    ("rs", "text/x-rust"),
    ("go", "text/x-golang"),
    ("java", "text/x-java"),
    ("ts", "text/x-typescript"),
    ("tsx", "text/tsx"),
    ("jsx", "text/jsx"),
    ("kt", "text/x-kotlin"),
    ("kts", "text/x-kotlin"),
    ("swift", "text/x-swift"),
    ("rb", "text/x-ruby"),
    ("php", "text/x-php"),
    ("cs", "text/x-csharp"),
    ("sh", "text/x-sh"),
    ("bash", "text/x-bash"),
    ("zsh", "text/x-zsh"),
    ("tex", "text/x-tex"),
    ("lua", "text/x-lua"),
    ("hs", "text/x-haskell"),
    ("ex", "text/x-elixir"),
    ("exs", "text/x-elixir"),
    ("erl", "text/x-erlang"),
    ("clj", "text/x-clojure"),
    ("scala", "text/x-scala"),
    ("r", "text/x-r"),
    ("jl", "text/x-julia"),
    ("dart", "text/x-dart"),
    ("mm", "text/x-objectivec++"),
    ("groovy", "text/x-groovy"),
    ("awk", "text/x-awk"),
    ("toml", "application/toml"),
    ("yaml", "application/yaml"),
    ("yml", "application/yaml"),
    ("ini", "text/x-ini"),
    ("properties", "text/x-properties"),
    ("proto", "text/x-protobuf"),
    ("graphql", "application/graphql"),
    ("gql", "application/graphql"),
    ("scss", "text/x-scss"),
    ("sass", "text/x-sass"),
    ("less", "text/x-less"),
    ("hcl", "text/x-hcl"),
    ("tf", "text/x-terraform"),
    ("ndjson", "application/x-ndjson"),
    ("jsonl", "application/x-ndjson"),
    ("json5", "application/json5"),
    ("diff", "text/x-diff"),
    ("patch", "text/x-patch"),
    ("cmake", "text/x-cmake"),
    ("gradle", "text/x-gradle"),
    ("astro", "text/x-astro"),
    ("hbs", "text/x-handlebars"),
    ("handlebars", "text/x-handlebars"),
    ("mustache", "text/x-mustache"),
    ("ejs", "text/x-ejs"),
    ("j2", "text/x-jinja2"),
    ("jinja", "text/x-jinja2"),
    ("jinja2", "text/x-jinja2"),
    ("liquid", "text/x-liquid"),
    ("erb", "text/x-erb"),
    ("twig", "text/x-twig"),
    ("pug", "text/x-pug"),
    ("jade", "text/x-jade"),
    ("tmpl", "text/x-tmpl"),
    ("ps1", "application/x-powershell"),
    ("lisp", "text/x-lisp"),
    ("el", "text/x-lisp"),
    ("vbs", "text/vbscript"),
    ("mk", "text/x-makefile"),
    ("makefile", "text/x-makefile"),
    ("dockerfile", "text/x-dockerfile"),
];

/// The MIME type to send a file with, if OpenAI's `input_file` takes it: its
/// own type when listed, else the type of its extension (or of a whole file
/// name such as `Makefile`).
pub(crate) fn openai_file_mime(mime: &str, filename: Option<&str>) -> Option<String> {
    let m = base_mime(mime);
    if OPENAI_FILE_MIMES.contains(&m.as_str()) {
        return Some(m);
    }
    let name = filename?.to_ascii_lowercase();
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_string()).unwrap_or(name);
    OPENAI_FILE_EXTS.iter().find(|(e, _)| *e == ext).map(|(_, m)| m.to_string())
}

#[allow(clippy::too_many_arguments)]
async fn openai_file(
    blob: &BlobRef,
    name: &str,
    filename: Option<&str>,
    class: MediaClass,
    inp: &Inputs<'_>,
    blobs: &dyn BlobLoader,
    content: &mut Vec<Value>,
    c: &mut Converted,
) -> Result<(), LlmError> {
    match openai_file_mime(&blob.mime, filename) {
        Some(mime) => {
            let bytes = provider::load(blobs, blob).await?;
            let mut f = json!({"type": "input_file", "filename": name, "file_data": provider::data_url(&mime, &bytes)});
            if mime == "application/pdf" {
                // Page images at the highest detail (`auto` is `low` before GPT-5.6).
                f["detail"] = json!("high");
                c.pdfs += 1;
            }
            content.push(f);
            c.files += 1;
        }
        None if class == MediaClass::Text => {
            let bytes = provider::load(blobs, blob).await?;
            let text =
                String::from_utf8(bytes).map_err(|_| LlmError::request(format!("{name} is not valid UTF-8 text")))?;
            content.push(json!({"type": "input_text", "text": text}));
        }
        None => {
            return Err(LlmError::request(format!(
                "`{}` does not take {} files via OpenAI ({name}): input_file takes PDF, Word/RTF/ODT, PowerPoint, \
                 Excel/CSV and text or code files",
                inp.model, blob.mime
            )));
        }
    }
    Ok(())
}

async fn xai_file(
    blob: &BlobRef,
    name: &str,
    class: MediaClass,
    inp: &Inputs<'_>,
    blobs: &dyn BlobLoader,
    content: &mut Vec<Value>,
    c: &mut Converted,
) -> Result<(), LlmError> {
    // docs.x.ai "Files": plain text, Markdown, code, CSV, JSON, PDF "and many other text-based formats".
    if !matches!(class, MediaClass::Pdf | MediaClass::Text) {
        return Err(LlmError::request(format!(
            "`{}` does not take {} files via xAI ({name}): its file input reads PDF and text-based files \
             (text, Markdown, code, CSV, JSON)",
            inp.model, blob.mime
        )));
    }
    let mime = match base_mime(&blob.mime).as_str() {
        "" | "application/octet-stream" if class == MediaClass::Pdf => "application/pdf".to_string(),
        "" | "application/octet-stream" => "text/plain".to_string(),
        _ => blob.mime.clone(),
    };
    let bytes = provider::load(blobs, blob).await?;
    // OpenAPI `input_file`: `file_data` is the base64 bytes, `filename` is required with it.
    content
        .push(json!({"type": "input_file", "filename": name, "file_data": provider::b64(&bytes), "mime_type": mime}));
    c.files += 1;
    if class == MediaClass::Pdf {
        c.pdfs += 1;
    }
    Ok(())
}

// ------------------------------------------------------------------ usage and cost

/// List prices per 1M tokens in USD (standard tier, short context).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input: f64,
    /// Cache reads.
    pub cached: f64,
    /// Cache writes with the 30-minute TTL (OpenAI GPT-5.6+ and GPT-6);
    /// 12-hour writes cost twice this. `None`: writes are billed as input.
    pub cache_write: Option<f64>,
    pub output: f64,
}

/// How a request's prices scale (long context, service tier).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Scale {
    pub input: f64,
    pub output: f64,
}

impl Scale {
    pub const ONE: Scale = Scale { input: 1.0, output: 1.0 };

    pub fn times(self, m: f64) -> Scale {
        Scale { input: self.input * m, output: self.output * m }
    }
}

/// Token cost in USD: uncached input, cache reads, cache writes (30-minute and
/// `written_12h` 12-hour ones) and output (reasoning included), scaled.
pub(crate) fn token_cost(p: &Price, u: &Usage, written_12h: u64, s: Scale) -> Option<f64> {
    let input = u.input_tokens? as f64;
    let output = u.output_tokens? as f64;
    let cached = u.cached_input_tokens.unwrap_or(0) as f64;
    let written = u.cache_write_tokens.unwrap_or(0) as f64;
    let w12 = (written_12h as f64).min(written);
    let w30 = written - w12;
    let uncached = (input - cached - written).max(0.0);
    let write = p.cache_write.unwrap_or(p.input);
    let inputs = uncached * p.input + cached * p.cached + w30 * write + w12 * 2.0 * write;
    Some((s.input * inputs + s.output * output * p.output) / 1e6)
}

/// Normalized usage of a Responses `usage` object (kept verbatim in `raw`).
///
/// `output_tokens` include the reasoning tokens on OpenAI and, in every xAI
/// response seen (2026-11-13: `total_tokens` = input + output), on xAI too;
/// xAI's REST reference example still shows them apart (`total_tokens` =
/// input + output + reasoning). The total tells which: reasoning reported
/// apart is added, so that output counts everything billed at the output
/// rate. Without a total, output is taken to include it. xAI reports the
/// billed cost (`cost_in_usd_ticks`, 10^10 per USD).
pub(crate) fn usage_from(u: &Value) -> Usage {
    let g = |p: &str| u.pointer(p).and_then(Value::as_u64);
    let input = g("/input_tokens");
    let output = g("/output_tokens");
    let reasoning = g("/output_tokens_details/reasoning_tokens");
    let reasoning_apart = match (g("/total_tokens"), input, output, reasoning) {
        (Some(t), Some(i), Some(o), Some(r)) if r > 0 => t == i + o + r,
        _ => false,
    };
    let output_tokens = match (output, reasoning) {
        (Some(o), Some(r)) if reasoning_apart => Some(o + r),
        (o, _) => o,
    };
    let cost = u
        .get("cost_in_usd_ticks")
        .and_then(Value::as_f64)
        .map(|t| t / 1e10)
        .or_else(|| u.get("cost_in_nano_usd").and_then(Value::as_f64).map(|n| n / 1e9));
    Usage {
        input_tokens: input,
        output_tokens,
        reasoning_tokens: reasoning,
        cached_input_tokens: g("/input_tokens_details/cached_tokens"),
        cache_write_tokens: g("/input_tokens_details/cache_write_tokens"),
        cost_usd: cost,
        cost_source: if cost.is_some() { CostSource::Reported } else { CostSource::Unknown },
        raw: u.clone(),
    }
}

/// Cost estimate of a finished response: `(model, response, normalized usage)`.
pub(crate) type Pricer = fn(&str, &Value, &Usage) -> Option<f64>;

/// The output items of a response, of one `type`.
pub(crate) fn items_of<'a>(response: &'a Value, ty: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
    response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(move |i| i.get("type").and_then(Value::as_str) == Some(ty))
}

/// Distinct code-interpreter containers a response used.
pub(crate) fn containers(response: &Value) -> usize {
    items_of(response, "code_interpreter_call")
        .filter_map(|i| i.get("container_id").and_then(Value::as_str))
        .collect::<BTreeSet<_>>()
        .len()
}

// ------------------------------------------------------------------ stream

/// A hosted tool call (output item) as shown: the platform's own tool name,
/// the query or code as input, the results as output.
pub(crate) fn tool_call(item: &Value) -> Option<ToolCall> {
    let ty = item.get("type").and_then(Value::as_str)?;
    let field = |k: &str| item.get(k).filter(|v| !v.is_null()).cloned();
    let named = |default: &str| item.get("name").and_then(Value::as_str).unwrap_or(default).to_string();
    let (name, input, output) = match ty {
        "web_search_call" => {
            // What the search returned is the call's result: every source consulted
            // (`include: web_search_call.action.sources`) and the image results
            // (`include: web_search_call.results`), verbatim.
            let mut action = field("action");
            let sources = action.as_mut().and_then(Value::as_object_mut).and_then(|a| a.remove("sources"));
            let mut out = serde_json::Map::new();
            if let Some(s) = sources.filter(|s| !s.is_null()) {
                out.insert("sources".into(), s);
            }
            if let Some(r) = field("results") {
                out.insert("results".into(), r);
            }
            ("web_search".to_string(), action, (!out.is_empty()).then_some(Value::Object(out)))
        }
        "code_interpreter_call" => ("code_interpreter".to_string(), field("code"), field("outputs").map(shown_outputs)),
        "file_search_call" => ("file_search".to_string(), field("queries"), field("results")),
        "custom_tool_call" => (named("custom_tool"), field("input"), field("output")),
        "mcp_call" => (named("mcp"), field("arguments"), field("output").or_else(|| field("error"))),
        "function_call" => (named("function"), field("arguments"), None),
        "image_generation_call" => {
            // The prompt the image model got (OpenAI's `revised_prompt`, xAI's `prompt`); what it
            // used (size, quality, background, format, action) or its error as the output.
            let mut used = item.as_object().cloned().unwrap_or_default();
            for k in ["id", "type", "status", "result", "revised_prompt", "prompt"] {
                used.remove(k);
            }
            used.retain(|_, v| !v.is_null());
            let input = field("revised_prompt").or_else(|| field("prompt"));
            ("image_generation".to_string(), input, (!used.is_empty()).then_some(Value::Object(used)))
        }
        t if t.ends_with("_call") => (t.trim_end_matches("_call").to_string(), None, None),
        _ => return None,
    };
    Some(ToolCall {
        id: item.get("id").and_then(Value::as_str).map(str::to_string),
        name,
        status: item.get("status").and_then(Value::as_str).map(str::to_string),
        input,
        output,
    })
}

/// Code outputs for display: images given as data URLs are shown as images
/// (see [`code_outputs`]), so their bytes are not repeated in the JSON.
fn shown_outputs(mut outputs: Value) -> Value {
    for o in outputs.as_array_mut().into_iter().flatten() {
        if let Some(url) = o.get("url").and_then(Value::as_str)
            && let Some((mime, bytes)) = decode_data_url(url)
        {
            o["url"] = json!(format!("data:{mime};base64,… ({} bytes, shown as an image)", bytes.len()));
        }
    }
    outputs
}

/// What a finished code run showed, in order: images given as data URLs
/// (`include: code_interpreter_call.outputs`) as images, images given by URL
/// fetched (with `files`, else nothing).
fn code_outputs(item: &Value, files: Option<&Fetcher>) -> Vec<Queued> {
    if item.get("type").and_then(Value::as_str) != Some("code_interpreter_call") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for o in item.get("outputs").and_then(Value::as_array).into_iter().flatten() {
        if o.get("type").and_then(Value::as_str) != Some("image") {
            continue;
        }
        let Some(url) = o.get("url").and_then(Value::as_str) else { continue };
        match decode_data_url(url) {
            Some((mime, data)) => {
                if let Some(f) = files {
                    f.shown(&data);
                }
                out.push(Queued::Event(StreamEvent::Image { data, mime }));
            }
            None if files.is_some() && (url.starts_with("https://") || url.starts_with("http://")) => {
                out.push(Queued::Fetch(Fetch::Image { url: url.to_string() }));
            }
            None => {}
        }
    }
    out
}

/// Retry for an error reported inside a stream (`response.failed`, `error`
/// events), by its code: rate limits wait, server-side trouble can pass;
/// anything else (policy blocks, invalid input, …) would fail again. Without
/// a code it is taken as transient.
pub(crate) fn in_band_retry(code: Option<&str>) -> Retry {
    let Some(c) = code.map(str::to_ascii_lowercase) else { return Retry::Backoff };
    if c.contains("rate_limit") || c.contains("rate-limit") {
        Retry::After(20)
    } else if ["server", "timeout", "overload", "unavailable", "internal"].iter().any(|k| c.contains(k)) {
        Retry::Backoff
    } else {
        Retry::Never
    }
}

/// The finish of a terminal event: `(normalized, native)`.
fn finish(ty: &str, r: &Value) -> (String, Option<String>) {
    if ty == "response.completed" {
        return ("stop".into(), r.get("status").and_then(Value::as_str).map(str::to_string));
    }
    let why = r.pointer("/incomplete_details/reason").and_then(Value::as_str).unwrap_or("incomplete");
    let norm = match why {
        "max_output_tokens" => "length",
        "content_filter" => "content_filter",
        // max_messages, steered (OpenAI); max_prompt_tokens, max_time_limit (xAI): as reported.
        other => other,
    };
    (norm.to_string(), Some(why.to_string()))
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// OpenAI's `input_transformations` (earlier inputs withheld from the model:
/// `{id, type: "reasoning", action: "removed", reason}`), when any was for a
/// reason other than `turn_boundary` (expected with `reasoning.context:
/// "current_turn"`): `organization_mismatch`, `end_user_mismatch`,
/// `model_mismatch`, `prefix_mismatch` (the conversation changed before it).
fn withheld(r: &Value) -> Option<String> {
    let mut reasons: Vec<(String, usize)> = Vec::new();
    for t in r.get("input_transformations").and_then(Value::as_array).into_iter().flatten() {
        let reason = t.get("reason").and_then(Value::as_str).unwrap_or("no reason given");
        if reason == "turn_boundary" {
            continue;
        }
        let what = match t.get("type").and_then(Value::as_str) {
            Some("reasoning") | None => reason.to_string(),
            Some(ty) => format!("{ty} {reason}"),
        };
        match reasons.iter_mut().find(|(w, _)| *w == what) {
            Some((_, n)) => *n += 1,
            None => reasons.push((what, 1)),
        }
    }
    let n: usize = reasons.iter().map(|(_, n)| n).sum();
    (n > 0).then(|| {
        let list: Vec<String> =
            reasons.iter().map(|(w, k)| if *k == 1 { w.clone() } else { format!("{w} ×{k}") }).collect();
        format!("ℹ️ OpenAI withheld {n} earlier reasoning item{} from the model: {}", plural(n), list.join(", "))
    })
}

/// OpenAI's `prompt_cache_diagnostics` against the previous turn, when the
/// cache missed because the conversation or the tools sent changed (the bot
/// replays them unchanged, so this shows a replay problem or a change of
/// tools); other outcomes stay in the response (`/info`).
fn cache_miss(r: &Value) -> Option<String> {
    let d = r.get("prompt_cache_diagnostics")?;
    if d.get("type").and_then(Value::as_str) != Some("cache_miss") {
        return None;
    }
    let reason = d.get("reason").and_then(Value::as_str)?;
    let what = match reason {
        "input_changed" => "the earlier conversation sent differs from the previous turn's",
        "tools_changed" => "the tools differ from the previous turn's",
        _ => return None,
    };
    let n = |k: &str| d.get(k).and_then(Value::as_u64);
    let counts = match (n("cache_missed_tokens"), n("comparison_reusable_tokens")) {
        (Some(m), Some(of)) => format!(", {m} of {of} reusable tokens missed"),
        (Some(m), None) => format!(", {m} tokens missed"),
        _ => String::new(),
    };
    Some(format!("ℹ️ prompt cache miss: {what} ({reason}{counts})"))
}

/// Streaming state of one call: OpenRouter's Responses parser plus what the
/// official APIs add.
pub(crate) struct Parser {
    inner: responses::EventParser,
    platform: Platform,
    model: String,
    price: Pricer,
    map_error: ErrorMapper,
    /// `id` of the response (`resp_…`), from `response.created`.
    pub response_id: Option<String>,
    /// Usage of a failed response (billed): emitted after the error.
    pub failed_usage: Option<Usage>,
    /// Image generation calls whose image was emitted.
    images_done: BTreeSet<String>,
    /// Image generation calls with a preview shown (until their image).
    previews: BTreeSet<String>,
    /// Fetches what the stream cites (OpenAI's container files; see [`super::files`]).
    files: Option<Fetcher>,
    /// Container files asked for (one cited twice is shown once).
    cited: BTreeSet<String>,
    /// The withheld inputs were noted.
    withheld_noted: bool,
}

impl Parser {
    pub fn new(platform: Platform, model: &str, price: Pricer, map_error: ErrorMapper) -> Self {
        Self {
            inner: responses::EventParser::default(),
            platform,
            model: model.to_string(),
            price,
            map_error,
            response_id: None,
            failed_usage: None,
            images_done: BTreeSet::new(),
            previews: BTreeSet::new(),
            files: None,
            cited: BTreeSet::new(),
            withheld_noted: false,
        }
    }

    /// Fetch the files the stream cites with `files`.
    pub fn with_files(mut self, files: Option<Fetcher>) -> Self {
        self.files = files;
        self
    }

    pub fn finished(&self) -> bool {
        self.inner.finished
    }

    pub fn any_output(&self) -> bool {
        crate::chatstream::EventParser::any_output(&self.inner) || !self.images_done.is_empty()
    }

    /// The image of a finished `image_generation_call` item (its base64
    /// `result`), once, and the end of its previews.
    fn generated_image(&mut self, item: &Value) -> Vec<StreamEvent> {
        if item.get("type").and_then(Value::as_str) != Some("image_generation_call") {
            return Vec::new();
        }
        let id = item.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let mut evs = Vec::new();
        if !self.images_done.contains(&id)
            && let Some(data) = item.get("result").and_then(Value::as_str).and_then(super::media::decode_b64)
        {
            let mime = super::media::sniff_image(&data, &super::images::format_mime(item.get("output_format")));
            evs.push(StreamEvent::Image { data, mime });
            self.images_done.insert(id.clone());
        }
        // Also when the call failed after previews: they end too.
        if self.previews.remove(&id) || !evs.is_empty() {
            evs.push(StreamEvent::PreviewDone { slot: id });
        }
        evs
    }

    /// `response.image_generation_call.*`: the call's progress, and its
    /// partial images (OpenAI, `partial_images`) as previews.
    fn image_progress(&mut self, ty: &str, v: &Value) -> Vec<StreamEvent> {
        let id = v.get("item_id").and_then(Value::as_str).unwrap_or_default().to_string();
        let what = ty.rsplit('.').next().unwrap_or_default();
        if what != "partial_image" {
            return vec![StreamEvent::Tool(ToolCall {
                id: Some(id),
                name: "image_generation".into(),
                status: Some(what.to_string()),
                input: None,
                output: None,
            })];
        }
        let Some(data) = v.get("partial_image_b64").and_then(Value::as_str).and_then(super::media::decode_b64) else {
            return Vec::new();
        };
        let mime = super::media::sniff_image(&data, &super::images::format_mime(v.get("output_format")));
        self.previews.insert(id.clone());
        vec![StreamEvent::PartialImage { data, mime, slot: id }]
    }

    /// The output items, for replay, and the response's `id` (OpenAI's prompt
    /// cache diagnostics compare the next request with it).
    pub fn native_payload(&self) -> Value {
        let mut p = self.inner.native_payload();
        if let Some(id) = &self.response_id {
            p["id"] = json!(id);
        }
        p
    }

    fn usage(&self, response: &Value, u: &Value) -> Usage {
        let mut usage = usage_from(u);
        if usage.cost_usd.is_none() {
            usage.cost_usd = (self.price)(&self.model, response, &usage);
            usage.cost_source = if usage.cost_usd.is_some() { CostSource::Estimated } else { CostSource::Unknown };
        }
        usage
    }

    /// The notice about inputs OpenAI withheld, once (they are reported from `response.in_progress` on).
    fn note_withheld(&mut self, r: &Value) -> Option<StreamEvent> {
        if self.withheld_noted {
            return None;
        }
        let n = withheld(r)?;
        self.withheld_noted = true;
        Some(StreamEvent::Notice(n))
    }

    /// `evs` in order with the fetches among them: handed to the fetcher as
    /// they are (they go out from there in this order, see [`Gate`]), or
    /// returned when nothing has to be fetched.
    fn release(&self, items: Vec<Queued>) -> Vec<StreamEvent> {
        match &self.files {
            Some(f) if items.iter().any(|i| matches!(i, Queued::Fetch(_))) => {
                f.enqueue(items);
                Vec::new()
            }
            _ => items
                .into_iter()
                .filter_map(|i| match i {
                    Queued::Event(e) => Some(e),
                    Queued::Fetch(_) => None,
                })
                .collect(),
        }
    }

    /// One SSE event.
    pub fn event(&mut self, v: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "response.failed" | "error" => return Err(self.in_band_error(ty, v)),
            "" if v.get("error").is_some_and(|e| !e.is_null()) => return Err(self.in_band_error(ty, v)),
            "response.completed" | "response.incomplete" => return self.terminal(ty, v),
            "response.created" | "response.in_progress" | "response.queued" => {
                if let Some(id) = v.pointer("/response/id").and_then(Value::as_str) {
                    self.response_id = Some(id.to_string());
                }
            }
            _ => {}
        }
        let mut evs = self.inner.event(v)?;
        if let Some(r) = v.get("response").filter(|r| r.is_object()) {
            evs.extend(self.note_withheld(r));
        }
        match ty {
            "response.output_item.added" | "response.output_item.done" => {
                evs.retain(|e| !matches!(e, StreamEvent::Tool(_)));
                if let Some(item) = v.get("item") {
                    if let Some(t) = tool_call(item) {
                        evs.push(StreamEvent::Tool(t));
                    }
                    if ty == "response.output_item.done" {
                        let mut items: Vec<Queued> = evs.drain(..).map(Queued::Event).collect();
                        items.extend(code_outputs(item, self.files.as_ref()));
                        items.extend(self.generated_image(item).into_iter().map(Queued::Event));
                        return Ok(self.release(items));
                    }
                }
            }
            t if t.starts_with("response.image_generation_call.") => evs.extend(self.image_progress(t, v)),
            "response.output_text.annotation.added" => {
                if self.platform == Platform::Xai {
                    for e in &mut evs {
                        if let StreamEvent::Citations(cs) = e {
                            cs.iter_mut().for_each(label_citation);
                        }
                    }
                }
                // A file the code wrote in its container: fetched now, shown here.
                if let Some(a) = v.get("annotation")
                    && a.get("type").and_then(Value::as_str) == Some("container_file_citation")
                    && let (Some(container_id), Some(file_id)) =
                        (a.get("container_id").and_then(Value::as_str), a.get("file_id").and_then(Value::as_str))
                    && self.files.is_some()
                    && self.cited.insert(format!("{container_id}/{file_id}"))
                {
                    let filename = a.get("filename").and_then(Value::as_str).unwrap_or(file_id).to_string();
                    let mut items: Vec<Queued> = evs.drain(..).map(Queued::Event).collect();
                    items.push(Queued::Fetch(Fetch::ContainerFile {
                        container_id: container_id.to_string(),
                        file_id: file_id.to_string(),
                        filename,
                    }));
                    return Ok(self.release(items));
                }
            }
            _ => {}
        }
        Ok(evs)
    }

    fn terminal(&mut self, ty: &str, v: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        // The inner parser keeps the final output items (replay) and marks the stream finished.
        let inner = self.inner.event(v)?;
        let r = v.get("response").unwrap_or(&Value::Null);
        // Images only in the final output (not in an `output_item.done` seen before).
        let mut evs: Vec<StreamEvent> =
            items_of(r, "image_generation_call").flat_map(|i| self.generated_image(i)).collect::<Vec<_>>();
        evs.extend(inner.into_iter().filter(|e| matches!(e, StreamEvent::FinishDetails(_))));
        evs.extend(self.note_withheld(r));
        evs.extend(cache_miss(r).map(StreamEvent::Notice));
        if let Some(u) = r.get("usage").filter(|u| u.is_object()) {
            if self.platform == Platform::Xai {
                // The conversation's size now (prompt and completion of the latest context), not
                // the billed sum over the internal calls of server-side tools.
                let ctx = |k: &str| u.pointer(&format!("/context_details/{k}")).and_then(Value::as_u64);
                if let (Some(i), Some(o)) = (ctx("input_tokens"), ctx("output_tokens")) {
                    evs.push(StreamEvent::ContextSize(i + o));
                }
                // Files are read by attachment_search only sometimes (not for inline files so far).
                if let Some(n) = u
                    .pointer("/server_side_tool_usage_details/document_search_calls")
                    .and_then(Value::as_u64)
                    .filter(|n| *n > 0)
                {
                    evs.push(StreamEvent::Notice(format!("📎 xAI searched the attached files (×{n})")));
                }
            }
            evs.push(StreamEvent::Usage(self.usage(r, u)));
        }
        let (reason, native) = finish(ty, r);
        evs.push(StreamEvent::Finish { reason: Some(reason), native });
        Ok(evs)
    }

    fn in_band_error(&mut self, ty: &str, v: &Value) -> LlmError {
        let err = match (ty, v.get("error")) {
            ("response.failed", _) => {
                v.pointer("/response/error").filter(|e| e.is_object()).cloned().unwrap_or_else(|| v.clone())
            }
            (_, Some(e)) if e.is_object() => e.clone(),
            // A bare error body in the stream: `{"code": "...", "error": "<message>"}`.
            (_, Some(Value::String(m))) => json!({"code": v.get("code"), "message": m}),
            // `{"type":"error","code","message","param","sequence_number"}`
            _ => json!({"code": v.get("code"), "message": v.get("message"), "param": v.get("param")}),
        };
        let mut e = (self.map_error)(&err, None, None, ErrorKind::InBand);
        e.partial_output = self.any_output();
        e.details = Some(v.clone());
        if let Some(r) = v.get("response")
            && let Some(u) = r.get("usage").filter(|u| u.is_object())
        {
            self.failed_usage = Some(self.usage(r, u));
        }
        e
    }
}

/// xAI labels inline citations by number (`title: "1"`): a label, not a title.
fn label_citation(c: &mut Citation) {
    if let Some(t) = &c.title
        && !t.is_empty()
        && t.bytes().all(|b| b.is_ascii_digit())
    {
        c.extra = Some(json!({"label": t}));
        c.title = None;
    }
}

impl crate::chatstream::EventParser for Parser {
    fn event(&mut self, _name: Option<&str>, data: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        Parser::event(self, data)
    }
    fn finished(&self) -> bool {
        Parser::finished(self)
    }
    fn any_output(&self) -> bool {
        Parser::any_output(self)
    }
}

/// How a platform's stream is read.
pub(crate) struct Wire {
    pub platform: Platform,
    pub price: Pricer,
    pub map_error: ErrorMapper,
    /// Final touch on errors (xAI: the error code from the body).
    pub refine: fn(LlmError) -> LlmError,
}

/// Per request of a container file fetch or a poll of a stored response.
const SIDE_TIMEOUT: Duration = Duration::from_secs(60);

/// How a stream is run: the API key, the idle timeout, and for OpenAI how a
/// broken stream is recovered from the stored response (`None`: never).
pub(crate) struct Run<'a> {
    pub http: &'a reqwest::Client,
    pub api_key: &'a str,
    pub idle_timeout: Duration,
    pub recovery: Option<&'a recover::Backoff>,
}

/// Run a prepared request: `POST`, then the SSE stream through [`Parser`];
/// on OpenAI the files the stream cites are fetched where cited, and a stream
/// that broke after the response started is recovered from OpenAI's stored
/// copy (see [`super::recover`]).
pub(crate) async fn stream(
    run: &Run<'_>,
    wire: &Wire,
    p: &Prepared,
    sink: &mut dyn EventSink,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> (Result<Value, LlmError>, CallOutcome) {
    let base = p.url.strip_suffix("/responses").unwrap_or(&p.url);
    let files = (wire.platform == Platform::OpenAi)
        .then(|| Fetcher::new(run.http.clone(), run.api_key, base, run.idle_timeout.min(SIDE_TIMEOUT)));
    let mut gate = Gate::new(sink, files.clone());
    let sent = Instant::now();
    let rb = run.http.post(&p.url).bearer_auth(run.api_key).json(&p.body);
    let parser = Parser::new(wire.platform, &p.model, wire.price, wire.map_error).with_files(files.clone());
    let crate::chatstream::Streamed { result, parser, header_id } = crate::chatstream::run_with(
        rb,
        &p.url,
        &p.body,
        run.idle_timeout,
        Some("x-request-id"),
        wire.map_error,
        parser,
        &mut gate,
        observer,
        cancel,
    )
    .await;
    gate.flush(cancel).await;
    let outcome = CallOutcome { upstream_id: parser.response_id.clone().or(header_id), ..Default::default() };
    let e = match result {
        // `data: [DONE]` without `response.completed`/`.incomplete`: cut off.
        Ok(()) if !parser.finished() => {
            let mut e = LlmError::network("stream ended without response.completed");
            e.partial_output = parser.any_output();
            e
        }
        Ok(()) => return (Ok(parser.native_payload()), outcome),
        Err(e) => e,
    };
    if let Some(u) = parser.failed_usage.clone() {
        gate.event(StreamEvent::Usage(u));
    }
    // Broken after the response started (not an error OpenAI reported, not a cancellation):
    // OpenAI may still finish it and store it.
    if let (Some(b), Some(id)) = (run.recovery, parser.response_id.as_deref())
        && matches!(e.kind, ErrorKind::Network | ErrorKind::Timeout)
    {
        let broke = sent.elapsed();
        let h = super::media::Http {
            client: run.http,
            api_key: run.api_key,
            idle_timeout: run.idle_timeout.min(SIDE_TIMEOUT),
            map_error: wire.map_error,
            refine: wire.refine,
            observer,
            cancel,
        };
        let include: Vec<String> = p
            .body
            .get("include")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|i| i.as_str().map(str::to_string))
            .collect();
        let url = format!("{}/{id}", p.url);
        match recover::stored(&h, &url, &include, b, recover::window(b, broke), cancel).await {
            Ok(Some(r)) => return (replay(&r, &e, broke, wire, p, files, &mut gate, cancel).await, outcome),
            Ok(None) => {}
            Err(cancelled) => return (Err(cancelled), outcome),
        }
    }
    (Err((wire.refine)(e)), outcome)
}

/// The stored copy `r` of a response whose stream broke with `e` after
/// `broke`: the call's output again (from a fresh parser, as a stream of it
/// would have given it), or the error it ended with.
#[allow(clippy::too_many_arguments)]
async fn replay(
    r: &Value,
    e: &LlmError,
    broke: Duration,
    wire: &Wire,
    p: &Prepared,
    files: Option<Fetcher>,
    gate: &mut Gate<'_>,
    cancel: &CancellationToken,
) -> Result<Value, LlmError> {
    let mut parser = Parser::new(wire.platform, &p.model, wire.price, wire.map_error).with_files(files);
    if r.get("status").and_then(Value::as_str) == Some("cancelled") {
        // No stream ends that way: nothing to show; what it used is billed.
        if let Some(u) = r.get("usage").filter(|u| u.is_object()) {
            gate.event(StreamEvent::Usage(parser.usage(r, u)));
        }
        let mut e = e.clone();
        e.message = format!("{} (OpenAI's stored copy of the response is cancelled)", e.message);
        return Err((wire.refine)(e));
    }
    gate.event(StreamEvent::Restart);
    gate.event(StreamEvent::Notice(format!(
        "ℹ️ the stream broke after {}s ({}): the answer is OpenAI's stored copy of the response",
        broke.as_secs(),
        e.message
    )));
    let mut failed = None;
    for ev in recover::events(r) {
        match parser.event(&ev) {
            Ok(evs) => evs.into_iter().for_each(|x| gate.event(x)),
            Err(err) => {
                failed = Some(err);
                break;
            }
        }
    }
    gate.flush(cancel).await;
    match failed {
        Some(err) => {
            if let Some(u) = parser.failed_usage.clone() {
                gate.event(StreamEvent::Usage(u));
            }
            Err((wire.refine)(err))
        }
        None => Ok(parser.native_payload()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use omni_core::content::{NativeOutput, ReasoningKind};
    use omni_core::{BlobRef, Origin};

    /// Blobs from memory.
    pub(crate) struct MemBlobs(pub Vec<(BlobRef, Vec<u8>)>);

    impl BlobLoader for MemBlobs {
        fn load<'a>(&'a self, r: &'a BlobRef) -> crate::BoxFuture<'a, anyhow::Result<Vec<u8>>> {
            let found = self.0.iter().find(|(b, _)| b == r).map(|(_, d)| d.clone());
            Box::pin(async move { found.ok_or_else(|| anyhow::anyhow!("no blob")) })
        }
    }

    pub(crate) fn file(name: &str, mime: &str, data: &[u8]) -> (Part, (BlobRef, Vec<u8>)) {
        let b = BlobRef::for_bytes(data, mime);
        (Part::File { blob: b.clone(), filename: Some(name.into()), generated: false }, (b, data.to_vec()))
    }

    pub(crate) fn photo(mime: &str, data: &[u8]) -> (Part, (BlobRef, Vec<u8>)) {
        let b = BlobRef::for_bytes(data, mime);
        (
            Part::Image { blob: b.clone(), filename: None, generated: false, dims: None, thought: false },
            (b, data.to_vec()),
        )
    }

    pub(crate) fn user(parts: Vec<Part>) -> Turn {
        Turn { role: Role::User, origin: Origin::Live, parts, native: None }
    }

    /// The request a [`serve`] server received.
    pub(crate) struct Captured {
        pub path: String,
        pub auth: Option<String>,
        pub body: Value,
    }

    pub(crate) type Captures = std::sync::Arc<std::sync::Mutex<Option<Captured>>>;

    /// A local HTTP server answering one request with `status`, the extra
    /// header lines and the body `chunks` (chunked encoding); returns its base
    /// URL and the captured request.
    pub(crate) async fn serve(
        status: u16,
        content_type: &str,
        headers: &str,
        chunks: Vec<String>,
    ) -> (String, Captures) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured: Captures = Default::default();
        let cap = captured.clone();
        let head = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\n{headers}transfer-encoding: chunked\r\n\r\n"
        );
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            let (req_head, body) = loop {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let req_head = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let len: usize = req_head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    while buf.len() < pos + 4 + len {
                        let n = sock.read(&mut tmp).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let end = (pos + 4 + len).min(buf.len());
                    break (req_head, serde_json::from_slice(&buf[pos + 4..end]).unwrap_or(Value::Null));
                }
            };
            let path = req_head.lines().next().and_then(|l| l.split(' ').nth(1)).unwrap_or("").to_string();
            let auth = req_head.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("authorization").then(|| v.trim().to_string())
            });
            *cap.lock().unwrap() = Some(Captured { path, auth, body });
            if sock.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            for c in chunks {
                let chunk = format!("{:x}\r\n{c}\r\n", c.len());
                if sock.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
            }
            let _ = sock.write_all(b"0\r\n\r\n").await;
            let _ = sock.flush().await;
        });
        (format!("http://{addr}/v1"), captured)
    }

    fn inputs(platform: Platform) -> Inputs<'static> {
        Inputs { platform, model: "m-1", image_detail: "high" }
    }

    fn price_none(_: &str, _: &Value, _: &Usage) -> Option<f64> {
        None
    }

    fn map_plain(err: &Value, status: Option<u16>, _: Option<u64>, kind: ErrorKind) -> LlmError {
        let mut e = LlmError::new(kind, crate::Retry::Never, err.get("message").and_then(Value::as_str).unwrap_or(""));
        e.status = status;
        e.error_type = err.get("code").and_then(Value::as_str).map(str::to_string);
        e
    }

    pub(crate) fn run(p: &mut Parser, events: &[Value]) -> (crate::Accumulator, Vec<StreamEvent>) {
        let mut acc = crate::Accumulator::new();
        let mut all = Vec::new();
        for e in events {
            for x in p.event(e).unwrap() {
                all.push(x.clone());
                acc.apply(x);
            }
        }
        (acc, all)
    }

    #[tokio::test]
    async fn history_replays_native_output_of_the_same_model_only() {
        let items = json!([
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "s"}], "encrypted_content": "gAAA"},
            {"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "q", "sources": [{"type": "url", "url": "https://a.b"}]}},
            {"type": "message", "id": "msg_1", "role": "assistant", "phase": "final_answer", "status": "completed", "content": [{"type": "output_text", "text": "hi", "annotations": []}]}
        ]);
        let native = |dialect: &str, model: &str| NativeOutput {
            dialect: dialect.into(),
            model: model.into(),
            payload: json!({"output": items, "content": "hi"}),
            context_tokens: None,
        };
        let mut a = Turn::assistant_text("hi");
        a.native = Some(native(super::super::DIALECT, "m-1"));
        let mut other_model = Turn::assistant_text("other");
        other_model.native = Some(native(super::super::DIALECT, "m-2"));
        let mut other_dialect = Turn::assistant_text("xai");
        other_dialect.native = Some(native(crate::xai::DIALECT, "m-1"));
        let system =
            Turn { role: Role::System, origin: Origin::Synthetic, parts: vec![Part::text("be brief")], native: None };
        let history = vec![system, Turn::user_text("q1"), a, Turn::user_text("q2"), other_model, other_dialect];
        let c = convert_history(&history, &inputs(Platform::OpenAi), &MemBlobs(vec![])).await.unwrap();
        assert_eq!(
            c.input[0],
            json!({"type": "message", "role": "system", "content": [{"type": "input_text", "text": "be brief"}]})
        );
        assert_eq!(
            c.input[1],
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "q1"}]})
        );
        // Verbatim, in order: reasoning with its encrypted content, the search call, the message with `phase`.
        assert_eq!(Value::Array(c.input[2..5].to_vec()), items);
        assert_eq!(c.input[6], json!({"type": "message", "role": "assistant", "content": "other"}));
        assert_eq!(c.input[7], json!({"type": "message", "role": "assistant", "content": "xai"}));
        assert_eq!(c.input.len(), 8);
    }

    #[tokio::test]
    async fn openai_files_go_natively_by_type() {
        let (pdf, b1) = file("a.pdf", "application/pdf", b"%PDF-1.7");
        let (rs, b2) = file("main.rs", "application/octet-stream", b"fn main() {}");
        let (xlsx, b3) =
            file("t.xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", b"PK\x03\x04");
        let (vue, b4) = file("App.vue", "application/octet-stream", b"<template/>");
        let (img, b5) = photo("image/jpeg", b"\xff\xd8\xff");
        let blobs = MemBlobs(vec![b1, b2, b3, b4, b5]);
        let history = vec![user(vec![Part::text("look"), pdf, rs, xlsx, vue, img])];
        let c = convert_history(&history, &inputs(Platform::OpenAi), &blobs).await.unwrap();
        let content = c.input[0]["content"].as_array().unwrap();
        assert_eq!(content[0], json!({"type": "input_text", "text": "look"}));
        assert_eq!(content[1]["type"], "input_file");
        assert_eq!(content[1]["filename"], "a.pdf");
        assert_eq!(content[1]["detail"], "high");
        assert!(content[1]["file_data"].as_str().unwrap().starts_with("data:application/pdf;base64,"));
        // A source file Telegram labels generically goes as its type (by extension).
        assert!(content[2]["file_data"].as_str().unwrap().starts_with("data:text/x-rust;base64,"));
        assert!(content[2].get("detail").is_none());
        assert!(
            content[3]["file_data"]
                .as_str()
                .unwrap()
                .starts_with("data:application/vnd.openxmlformats-officedocument.spreadsheetml.sheet;base64,")
        );
        // A text type input_file does not list: inlined as its own part.
        assert_eq!(content[4], json!({"type": "input_text", "text": "<template/>"}));
        assert_eq!(content[5]["type"], "input_image");
        assert_eq!(content[5]["detail"], "high");
        assert!(content[5]["image_url"].as_str().unwrap().starts_with("data:image/jpeg;base64,"));
        assert!(c.notes.is_empty(), "{:?}", c.notes);
        assert_eq!((c.images, c.pdfs, c.files), (1, 1, 3));
        assert_eq!(openai_file_mime("text/x-R", None).as_deref(), Some("text/x-r"));
        assert_eq!(openai_file_mime("application/octet-stream", Some("Makefile")).as_deref(), Some("text/x-makefile"));
        assert_eq!(openai_file_mime("application/zip", Some("a.zip")), None);
    }

    #[tokio::test]
    async fn unsupported_inputs_are_errors() {
        let ogg = file("voice.ogg", "audio/ogg", b"OggS");
        let mp4 = file("v.mp4", "video/mp4", b"....");
        let zip = file("a.zip", "application/zip", b"PK");
        let heic = photo("image/heic", b"....");
        let webp = photo("image/webp", b"RIFF");
        let docx = file("a.docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document", b"PK");
        let loc = Part::Location { latitude: 1.0, longitude: 2.0, accuracy_m: None, title: None, address: None };
        let yt =
            Part::RemoteMedia { url: "https://youtu.be/x".into(), kind: omni_core::content::RemoteMediaKind::YouTube };
        let blobs = MemBlobs(vec![ogg.1, mp4.1, zip.1, heic.1, webp.1.clone(), docx.1.clone()]);
        let err = |p: Part, platform: Platform| {
            let h = vec![user(vec![p])];
            let blobs = &blobs;
            async move { convert_history(&h, &inputs(platform), blobs).await.unwrap_err().message }
        };
        assert!(err(ogg.0, Platform::OpenAi).await.contains("does not take audio via OpenAI (voice.ogg, audio/ogg)"));
        assert!(err(mp4.0, Platform::Xai).await.contains("does not take video via xAI"));
        assert!(err(zip.0.clone(), Platform::OpenAi).await.contains("does not take application/zip files via OpenAI"));
        assert!(err(heic.0, Platform::OpenAi).await.contains("PNG, JPEG, WEBP or non-animated GIF only"));
        assert!(err(webp.0.clone(), Platform::Xai).await.contains("JPEG or PNG only"));
        assert!(err(docx.0.clone(), Platform::Xai).await.contains("reads PDF and text-based files"));
        assert!(err(loc, Platform::OpenAi).await.contains("does not take a location via OpenAI"));
        assert!(err(yt, Platform::Xai).await.contains("cannot read remote media via xAI"));
        // The same files are fine where the API takes them.
        let h = vec![user(vec![webp.0, docx.0])];
        assert!(convert_history(&h, &inputs(Platform::OpenAi), &blobs).await.is_ok());
    }

    #[tokio::test]
    async fn xai_files_are_inline_base64_with_a_name() {
        let (pdf, b1) = file("r.pdf", "application/pdf", b"%PDF");
        let (txt, b2) = file("notes.md", "application/octet-stream", b"# hi");
        let (big, _) = {
            let b = BlobRef { sha256: "0".repeat(64), size: 21 * 1024 * 1024, mime: "image/png".into() };
            (Part::Image { blob: b.clone(), filename: None, generated: false, dims: None, thought: false }, b)
        };
        let blobs = MemBlobs(vec![b1, b2]);
        let c = convert_history(&[user(vec![pdf, txt])], &inputs(Platform::Xai), &blobs).await.unwrap();
        let content = c.input[0]["content"].as_array().unwrap();
        assert_eq!(
            content[0],
            json!({"type": "input_file", "filename": "r.pdf", "file_data": provider::b64(b"%PDF"), "mime_type": "application/pdf"})
        );
        assert_eq!(content[1]["mime_type"], "text/plain");
        assert_eq!(content[1]["file_data"], provider::b64(b"# hi"));
        assert!(c.notes.is_empty());
        let e = convert_history(&[user(vec![big])], &inputs(Platform::Xai), &blobs).await.unwrap_err();
        assert!(e.message.contains("over 20 MiB"), "{}", e.message);
    }

    #[test]
    fn usage_is_normalized_across_the_two_apis() {
        // OpenAI: output includes reasoning (total = input + output).
        let u = usage_from(
            &json!({"input_tokens": 75, "input_tokens_details": {"cached_tokens": 10, "cache_write_tokens": 5},
                    "output_tokens": 1186, "output_tokens_details": {"reasoning_tokens": 1024}, "total_tokens": 1261}),
        );
        assert_eq!((u.input_tokens, u.output_tokens, u.reasoning_tokens), (Some(75), Some(1186), Some(1024)));
        assert_eq!((u.cached_input_tokens, u.cache_write_tokens), (Some(10), Some(5)));
        assert_eq!((u.cost_usd, u.cost_source), (None, CostSource::Unknown));
        // xAI as it answers (turn 238, grok-4.7): output includes reasoning too; the cost is billed in ticks.
        let x = usage_from(
            &json!({"input_tokens": 1254, "input_tokens_details": {"cached_tokens": 1152}, "output_tokens": 1324,
                    "output_tokens_details": {"reasoning_tokens": 1322}, "total_tokens": 2578, "num_sources_used": 0,
                    "num_server_side_tools_used": 0, "cost_in_usd_ticks": 87240000,
                    "context_details": {"input_tokens": 1254, "output_tokens": 1331}}),
        );
        assert_eq!((x.output_tokens, x.reasoning_tokens), (Some(1324), Some(1322)));
        assert!((x.cost_usd.unwrap() - 0.008724).abs() < 1e-12);
        assert_eq!(x.cost_source, CostSource::Reported);
        // xAI's REST reference example counts reasoning apart (total = input + output + reasoning): added.
        let x =
            usage_from(&json!({"input_tokens": 32, "input_tokens_details": {"cached_tokens": 8}, "output_tokens": 9,
                    "output_tokens_details": {"reasoning_tokens": 110}, "total_tokens": 151,
                    "num_sources_used": 0, "num_server_side_tools_used": 0, "cost_in_usd_ticks": 37756000}));
        assert_eq!((x.output_tokens, x.reasoning_tokens), (Some(119), Some(110)));
        assert_eq!(x.raw["output_tokens"], 9, "raw stays verbatim");
        // Without a total: output is taken to include the reasoning (as in every response seen).
        let x = usage_from(
            &json!({"input_tokens": 5, "output_tokens": 40, "output_tokens_details": {"reasoning_tokens": 30}}),
        );
        assert_eq!(x.output_tokens, Some(40));
    }

    #[test]
    fn token_cost_counts_every_kind_of_token() {
        let p = Price { input: 2.0, cached: 0.1, cache_write: Some(2.5), output: 10.0 };
        let u = Usage {
            input_tokens: Some(1_000_000),
            cached_input_tokens: Some(400_000),
            cache_write_tokens: Some(100_000),
            output_tokens: Some(100_000),
            ..Default::default()
        };
        // 0.5M × 2 + 0.4M × 0.1 + 0.06M × 2.5 + 0.04M × 5 (12h) + 0.1M × 10
        let c = token_cost(&p, &u, 40_000, Scale::ONE).unwrap();
        assert!((c - (1.0 + 0.04 + 0.15 + 0.2 + 1.0)).abs() < 1e-9, "{c}");
        let s = Scale { input: 2.0, output: 1.5 }.times(0.5);
        let c2 = token_cost(&p, &u, 40_000, s).unwrap();
        // inputs × 2 (long context) × 0.5 (tier), output × 1.5 × 0.5
        assert!((c2 - (1.39 * 1.0 + 1.0 * 0.75)).abs() < 1e-9, "{c2}");
    }

    #[test]
    fn stream_with_search_sources_code_outputs_and_citations() {
        let png = provider::b64(b"\x89PNG\r\n");
        let mut p = Parser::new(Platform::OpenAi, "m-1", price_none, map_plain);
        let events = [
            json!({"type": "response.created", "sequence_number": 0, "response": {"id": "resp_1", "status": "in_progress", "model": "m-1-2026-10-01"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": []}}),
            json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "delta": "**Planning**"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "**Planning**"}], "encrypted_content": "gAAA"}}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "web_search_call", "id": "ws_1", "status": "in_progress"}}),
            json!({"type": "response.web_search_call.searching", "item_id": "ws_1", "output_index": 1}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "web_search_call", "id": "ws_1", "status": "completed",
                "action": {"type": "search", "query": "paris", "queries": ["paris"], "sources": [{"type": "url", "url": "https://fr.example"}]}}}),
            json!({"type": "response.output_item.added", "output_index": 2, "item": {"type": "code_interpreter_call", "id": "ci_1", "status": "in_progress", "container_id": "cntr_1"}}),
            json!({"type": "response.code_interpreter_call_code.delta", "item_id": "ci_1", "output_index": 2, "delta": "plot()"}),
            json!({"type": "response.output_item.done", "output_index": 2, "item": {"type": "code_interpreter_call", "id": "ci_1", "status": "completed", "container_id": "cntr_1", "code": "plot()",
                "outputs": [{"type": "logs", "logs": "ok"}, {"type": "image", "url": format!("data:image/png;base64,{png}")}]}}),
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 3, "content_index": 0, "delta": "Paris", "logprobs": []}),
            json!({"type": "response.output_text.annotation.added", "item_id": "msg_1", "output_index": 3, "content_index": 0, "annotation_index": 0,
                "annotation": {"type": "url_citation", "url": "https://fr.example", "title": "France", "start_index": 0, "end_index": 5}}),
            json!({"type": "response.output_text.annotation.added", "item_id": "msg_1", "output_index": 3, "content_index": 0, "annotation_index": 1,
                "annotation": {"type": "container_file_citation", "container_id": "cntr_1", "file_id": "cfile_1", "filename": "plot.png", "start_index": 5, "end_index": 5}}),
        ];
        let (acc, all) = run(&mut p, &events);
        assert_eq!(p.response_id.as_deref(), Some("resp_1"));
        assert!(all.iter().any(|e| matches!(e, StreamEvent::Meta { model: Some(m), .. } if m == "m-1-2026-10-01")));
        let parts = acc.parts();
        assert!(
            matches!(&parts[0], Part::Reasoning(r) if r.kind == ReasoningKind::Summary && r.text.as_deref() == Some("**Planning**"))
        );
        let Part::ToolCall(ws) = &parts[1] else { panic!("{:?}", parts[1]) };
        assert_eq!(ws.name, "web_search");
        assert_eq!(ws.status.as_deref(), Some("completed"));
        assert_eq!(ws.input, Some(json!({"type": "search", "query": "paris", "queries": ["paris"]})));
        assert_eq!(ws.output, Some(json!({"sources": [{"type": "url", "url": "https://fr.example"}]})));
        let Part::ToolCall(ci) = &parts[2] else { panic!("{:?}", parts[2]) };
        assert_eq!((ci.name.as_str(), ci.input.clone()), ("code_interpreter", Some(json!("plot()"))));
        let out = ci.output.as_ref().unwrap();
        assert_eq!(out[0], json!({"type": "logs", "logs": "ok"}));
        assert_eq!(out[1]["url"], "data:image/png;base64,… (6 bytes, shown as an image)");
        assert!(
            all.iter().any(
                |e| matches!(e, StreamEvent::Image { mime, data } if mime == "image/png" && data == b"\x89PNG\r\n")
            )
        );
        let Part::Text { text, citations } = &parts[3] else { panic!("{:?}", parts[3]) };
        assert_eq!((text.as_str(), citations.len(), citations[0].title.as_deref()), ("Paris", 1, Some("France")));
        // The container file is fetched by the stream (see `container_files_are_shown_where_cited`):
        // a parser without a fetcher shows nothing for it.
        assert!(acc.notices.is_empty(), "{:?}", acc.notices);
        assert!(!p.finished());
    }

    #[test]
    fn terminal_events_bring_usage_and_finish() {
        fn price(_: &str, r: &Value, u: &Usage) -> Option<f64> {
            Some(u.input_tokens? as f64 + if r["service_tier"] == "flex" { 0.5 } else { 0.0 })
        }
        let mut p = Parser::new(Platform::OpenAi, "m-1", price, map_plain);
        let evs = p
            .event(&json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed", "service_tier": "flex",
                "output": [{"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": "x"}]}],
                "usage": {"input_tokens": 3, "output_tokens": 4, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 7}}}))
            .unwrap();
        assert!(
            matches!(&evs[0], StreamEvent::Usage(u) if u.cost_usd == Some(3.5) && u.cost_source == CostSource::Estimated)
        );
        assert!(
            matches!(&evs[1], StreamEvent::Finish { reason: Some(r), native: Some(n) } if r == "stop" && n == "completed")
        );
        assert!(p.finished());
        assert_eq!(p.native_payload()["output"][0]["id"], "msg_1");

        let mut p = Parser::new(Platform::Xai, "m-1", price, map_plain);
        let evs = p
            .event(&json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": "max_time_limit"}, "output": []}}))
            .unwrap();
        assert!(matches!(&evs[0], StreamEvent::FinishDetails(d) if d["reason"] == "max_time_limit"));
        assert!(matches!(&evs[1], StreamEvent::Finish { reason: Some(r), .. } if r == "max_time_limit"));
        let mut p = Parser::new(Platform::OpenAi, "m-1", price, map_plain);
        let evs = p
            .event(&json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": "content_filter"}, "output": []}}))
            .unwrap();
        assert!(
            matches!(&evs[1], StreamEvent::Finish { reason: Some(r), native: Some(n) } if r == "content_filter" && n == "content_filter")
        );
    }

    #[test]
    fn in_band_errors_use_the_platform_mapper_and_keep_billed_usage() {
        let mut p = Parser::new(Platform::OpenAi, "m-1", price_none, map_plain);
        let failed = json!({"type": "response.failed", "response": {"id": "resp_1", "status": "failed",
            "error": {"code": "server_error", "message": "The model failed to generate a response."},
            "usage": {"input_tokens": 5, "output_tokens": 0, "total_tokens": 5}}});
        let e = p.event(&failed).unwrap_err();
        assert_eq!((e.kind, e.error_type.as_deref()), (ErrorKind::InBand, Some("server_error")));
        assert_eq!(e.message, "The model failed to generate a response.");
        assert_eq!(e.details, Some(failed));
        assert_eq!(p.failed_usage.as_ref().and_then(|u| u.input_tokens), Some(5));
        let e = p
            .event(&json!({"type": "error", "code": "ERR_SOMETHING", "message": "Something went wrong", "param": null, "sequence_number": 1}))
            .unwrap_err();
        assert_eq!((e.error_type.as_deref(), e.message.as_str()), (Some("ERR_SOMETHING"), "Something went wrong"));
        // A bare error body as a stream line is an error too (not a dropped stream).
        let bare = json!({"code": "invalid-argument", "error": "Bad request"});
        let e = p.event(&bare).unwrap_err();
        assert_eq!((e.error_type.as_deref(), e.message.as_str()), (Some("invalid-argument"), "Bad request"));
        assert_eq!(e.details, Some(bare));
    }

    #[test]
    fn refusals_stay_refusals() {
        // Shapes of the streaming-events reference (`response.refusal.delta` / `.done`).
        let mut p = Parser::new(Platform::OpenAi, "m-1", price_none, map_plain);
        let (acc, _) = run(
            &mut p,
            &[
                json!({"type": "response.output_item.added", "output_index": 0, "sequence_number": 0,
                    "item": {"type": "message", "id": "msg_123", "status": "in_progress", "role": "assistant", "content": []}}),
                json!({"type": "response.refusal.delta", "item_id": "msg_123", "output_index": 0, "content_index": 0, "delta": "I can't help", "sequence_number": 1}),
                json!({"type": "response.refusal.delta", "item_id": "msg_123", "output_index": 0, "content_index": 0, "delta": " with that.", "sequence_number": 2}),
                json!({"type": "response.refusal.done", "item_id": "msg_123", "output_index": 0, "content_index": 0, "refusal": "I can't help with that.", "sequence_number": 3}),
                json!({"type": "response.output_text.annotation.added", "item_id": "msg_123", "output_index": 0, "content_index": 0, "annotation_index": 0,
                    "annotation": {"type": "file_citation", "file_id": "file-abc", "index": 0, "filename": "example.txt"}, "sequence_number": 4}),
                json!({"type": "response.completed", "sequence_number": 5, "response": {"id": "resp_1", "status": "completed",
                    "output": [{"type": "message", "id": "msg_123", "role": "assistant", "status": "completed", "content": [{"type": "refusal", "refusal": "I can't help with that."}]}]}}),
            ],
        );
        assert_eq!(acc.parts(), &[Part::Refusal { text: "I can't help with that.".into() }]);
        assert_eq!(acc.finish_reason.as_deref(), Some("stop"));
        assert_eq!(p.native_payload()["output"][0]["content"][0]["type"], "refusal");
    }

    #[test]
    fn xai_numbered_citations_are_labels() {
        let mut p = Parser::new(Platform::Xai, "m-1", price_none, map_plain);
        let (acc, _) = run(
            &mut p,
            &[
                json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": "xAI[[1]](https://x.ai/company)"}),
                json!({"type": "response.output_text.annotation.added", "output_index": 0, "content_index": 0,
                    "annotation": {"type": "url_citation", "url": "https://x.ai/company", "start_index": 3, "end_index": 30, "title": "1"}}),
            ],
        );
        let Part::Text { citations, .. } = &acc.parts()[0] else { panic!() };
        assert_eq!(citations[0].url.as_deref(), Some("https://x.ai/company"));
        assert_eq!((citations[0].title.clone(), citations[0].extra.clone()), (None, Some(json!({"label": "1"}))));
    }

    #[test]
    fn recorded_openai_streams_parse() {
        // Real OpenAI event streams (recorded through OpenRouter, 2026-11-11).
        for (case, native_reason) in
            [("rsp_length", "max_output_tokens"), ("rsp_reasoning_ate_budget", "max_output_tokens")]
        {
            let rec = crate::fixtures::case(case);
            let mut p = Parser::new(Platform::OpenAi, "gpt-6-luna", price_none, map_plain);
            let (acc, _) = run(&mut p, rec["sse"].as_array().unwrap());
            assert!(p.finished(), "{case}");
            assert_eq!(acc.finish_reason.as_deref(), Some("length"), "{case}");
            assert_eq!(acc.native_finish_reason.as_deref(), Some(native_reason), "{case}");
            assert!(acc.usage.as_ref().and_then(|u| u.output_tokens).is_some(), "{case}");
            assert!(!p.native_payload()["output"].as_array().unwrap().is_empty(), "{case}");
        }
    }
}
