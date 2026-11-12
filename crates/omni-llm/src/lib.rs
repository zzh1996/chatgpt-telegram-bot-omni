//! LLM provider abstraction.
//!
//! - [`sse`]: SSE parser.
//! - [`error`]: errors with retry classification; upstream payloads kept verbatim.
//! - [`event`]: normalized stream events and the turn accumulator.
//! - [`openrouter`]: OpenRouter client (catalog, Chat Completions / Responses /
//!   Messages skins, options).
//! - [`provider`]: the platforms' own APIs behind one interface
//!   ([`provider::Provider`]): [`openai`], [`anthropic`], [`google`], [`xai`],
//!   [`deepseek`], [`zhipu`], [`moonshot`], [`alibaba`], [`ark`].
#![allow(clippy::result_large_err)]

pub mod alibaba;
pub mod anthropic;
pub mod ark;
pub mod budget;
pub mod chatstream;
pub mod deepseek;
pub mod error;
pub mod event;
#[cfg(test)]
mod fixtures;
pub mod google;
pub mod moonshot;
pub mod openai;
pub mod openrouter;
pub mod provider;
pub mod request;
pub mod settings;
pub mod sse;
pub mod xai;
pub mod zhipu;

use std::future::Future;
use std::pin::Pin;

use omni_core::trigger::OptionToken;
use omni_core::{BlobRef, Turn};
use serde_json::Value;

pub use error::{ErrorKind, LlmError, Retry};
pub use event::{Accumulator, StreamEvent};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Loads blob bytes for request building.
pub trait BlobLoader: Send + Sync {
    fn load<'a>(&'a self, r: &'a BlobRef) -> BoxFuture<'a, anyhow::Result<Vec<u8>>>;
}

/// Receives raw wire traffic for verbose logging (implementations redact blobs).
pub trait Observer: Send + Sync {
    fn request(&self, url: &str, body: &Value);
    fn response_status(&self, status: u16, headers: &[(String, String)]);
    fn sse_comment(&self, comment: &str);
    fn sse_data(&self, data: &str);
    fn body_text(&self, text: &str);
}

/// No-op observer.
pub struct NullObserver;
impl Observer for NullObserver {
    fn request(&self, _: &str, _: &Value) {}
    fn response_status(&self, _: u16, _: &[(String, String)]) {}
    fn sse_comment(&self, _: &str) {}
    fn sse_data(&self, _: &str) {}
    fn body_text(&self, _: &str) {}
}

/// Receives normalized events as they arrive.
pub trait EventSink: Send {
    fn event(&mut self, ev: StreamEvent);
}

impl<F: FnMut(StreamEvent) + Send> EventSink for F {
    fn event(&mut self, ev: StreamEvent) {
        self(ev)
    }
}

/// Everything needed to make one call.
pub struct CallInput<'a> {
    /// Model id as known by the provider (e.g. `anthropic/claude-opus-5.5`).
    pub model: &'a str,
    pub options: &'a [OptionToken],
    /// Full history in order; usually ends with a user turn.
    pub history: &'a [Turn],
    /// Stable conversation key (sticky routing / caching).
    pub session_id: Option<String>,
    /// Stable end-user identifier (already hashed).
    pub user: Option<String>,
    /// Exact token count of a prefix of `history` (from earlier usage), for budgeting.
    pub known_context: Option<budget::KnownContext>,
    /// Force this `max_tokens` (after a context-overflow error), unless the user set one.
    pub max_tokens_override: Option<u64>,
}

/// A fully built request, inspectable before sending.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub dialect: &'static str,
    pub model: String,
    pub url: String,
    pub body: Value,
    /// Patches that were applied, in order, with their source (for transparency).
    pub applied: Vec<openrouter::options::Applied>,
    pub warnings: Vec<String>,
    /// Notes about input conversion (e.g. text file inlined).
    pub notes: Vec<String>,
    pub show_debug: bool,
    /// The `max_tokens` the bot chose automatically (None if the user set it or none was set).
    pub auto_max_tokens: Option<u64>,
    /// The model supports long decode (continuation hops).
    pub long_decode: bool,
    /// What the request asks for, in words (footer): effort, tools, options.
    pub settings: Vec<String>,
}

impl Prepared {
    /// Request that resumes a long-decode answer from a continuation item. On
    /// OpenRouter: the same conversation plus an assistant turn carrying only
    /// that item; on the Gemini API: see [`google::continuation_request`].
    pub fn with_continuation(&self, item: &Value) -> Prepared {
        if self.dialect == google::DIALECT {
            return google::continuation_request(self, item);
        }
        let mut p = self.clone();
        if let Some(msgs) = p.body.get_mut("messages").and_then(Value::as_array_mut) {
            msgs.push(serde_json::json!({"role": "assistant", "content": "", "reasoning_details": [item]}));
        }
        p
    }

    /// Where the request's output budget lives in its body.
    fn max_tokens_pointer(&self) -> &'static str {
        if self.dialect == google::DIALECT {
            "/generationConfig/maxOutputTokens"
        } else if self.body.get("max_output_tokens").is_some() || self.dialect.ends_with(".responses") {
            "/max_output_tokens"
        } else if self.body.get("max_completion_tokens").is_some() {
            "/max_completion_tokens"
        } else {
            "/max_tokens"
        }
    }

    /// The output budget of the request (`max_tokens`, Responses' `max_output_tokens`,
    /// Gemini's `maxOutputTokens`, …).
    pub fn max_tokens(&self) -> Option<u64> {
        self.body.pointer(self.max_tokens_pointer()).and_then(Value::as_u64)
    }

    /// Set (`Some`) or remove the output budget, wherever the dialect keeps it.
    pub fn set_max_tokens(&mut self, v: Option<u64>) {
        let ptr = self.max_tokens_pointer();
        let (parent, key) = ptr.rsplit_once('/').unwrap_or(("", ptr));
        let key = key.to_string();
        if parent.is_empty() {
            match (v, self.body.as_object_mut()) {
                (Some(v), Some(o)) => {
                    o.insert(key, v.into());
                }
                (None, Some(o)) => {
                    o.remove(&key);
                }
                _ => {}
            }
            return;
        }
        let parent = parent.trim_start_matches('/').to_string();
        match v {
            Some(v) => {
                if !self.body.get(&parent).is_some_and(Value::is_object) {
                    self.body[&parent] = serde_json::json!({});
                }
                self.body[&parent][&key] = v.into();
            }
            None => {
                if let Some(o) = self.body.get_mut(&parent).and_then(Value::as_object_mut) {
                    o.remove(&key);
                }
            }
        }
    }
}

/// One native payload for replay from those of several long-decode hops
/// (`full_text`: the whole answer).
pub fn merge_natives(dialect: &str, natives: &[Value], full_text: &str) -> Value {
    if dialect == google::DIALECT {
        return google::merge_natives(natives, full_text);
    }
    // Responses output items and Messages content blocks are complete per call
    // (only Chat Completions answers come in several long-decode hops): kept
    // exactly as received, or their replay would lose reasoning and tool blocks.
    if !dialect.ends_with(".chat") {
        return natives.last().cloned().unwrap_or(Value::Null);
    }
    let mut m = natives.last().cloned().unwrap_or_else(|| serde_json::json!({"role": "assistant"}));
    m["content"] = serde_json::json!(full_text);
    let details: Vec<Value> = natives
        .iter()
        .filter_map(|n| n.get("reasoning_details").and_then(Value::as_array))
        .flatten()
        .cloned()
        .collect();
    if !details.is_empty() {
        m["reasoning_details"] = Value::Array(details);
    }
    let reasoning: String = natives.iter().filter_map(|n| n.get("reasoning").and_then(Value::as_str)).collect();
    if !reasoning.is_empty() {
        m["reasoning"] = serde_json::json!(reasoning);
    }
    m
}

/// Result metadata of a finished (or failed) stream.
#[derive(Debug, Clone, Default)]
pub struct CallOutcome {
    pub upstream_id: Option<String>,
    /// Latest long-decode continuation item of this stream.
    pub continuation: Option<Value>,
    /// Exact upstream request bodies echoed by the router (OpenRouter `debug`).
    pub upstream_echo: Vec<Value>,
    pub router_metadata: Option<Value>,
}

/// Coarse classification of a MIME type for request building.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaClass {
    Image,
    Pdf,
    /// Office documents etc. that some providers accept natively.
    Document,
    Audio,
    Video,
    Text,
    Other,
}

pub fn classify_mime(mime: &str, filename: Option<&str>) -> MediaClass {
    let m = mime.to_ascii_lowercase();
    let ext = filename.and_then(|f| f.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    if m.starts_with("image/") {
        return MediaClass::Image;
    }
    if m == "application/pdf" || ext == "pdf" {
        return MediaClass::Pdf;
    }
    if m.starts_with("audio/") {
        return MediaClass::Audio;
    }
    if m.starts_with("video/") {
        return MediaClass::Video;
    }
    if m.starts_with("text/")
        || matches!(
            m.as_str(),
            "application/json"
                | "application/xml"
                | "application/x-yaml"
                | "application/yaml"
                | "application/toml"
                | "application/javascript"
                | "application/x-sh"
                | "application/x-python"
                | "application/sql"
        )
    {
        return MediaClass::Text;
    }
    if m.contains("officedocument") || m == "application/msword" || m == "application/vnd.ms-excel" {
        return MediaClass::Document;
    }
    const TEXT_EXT: &[&str] = &[
        "txt",
        "md",
        "markdown",
        "rs",
        "py",
        "js",
        "ts",
        "tsx",
        "jsx",
        "go",
        "c",
        "h",
        "cc",
        "cpp",
        "hpp",
        "java",
        "kt",
        "swift",
        "rb",
        "php",
        "cs",
        "sh",
        "bash",
        "zsh",
        "fish",
        "toml",
        "yaml",
        "yml",
        "json",
        "jsonl",
        "xml",
        "html",
        "htm",
        "css",
        "scss",
        "sql",
        "csv",
        "tsv",
        "log",
        "ini",
        "cfg",
        "conf",
        "tex",
        "lua",
        "hs",
        "ml",
        "ex",
        "exs",
        "erl",
        "clj",
        "scala",
        "r",
        "jl",
        "dart",
        "vue",
        "svelte",
        "proto",
        "graphql",
        "dockerfile",
        "makefile",
        "cmake",
        "gradle",
        "patch",
        "diff",
        "srt",
        "vtt",
    ];
    if TEXT_EXT.contains(&ext.as_str()) {
        return MediaClass::Text;
    }
    MediaClass::Other
}
