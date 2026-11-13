//! Files that code execution writes, fetched from the Files API as soon as
//! the tool result naming them arrives, so they appear right after it (D30).
//!
//! - A `bash_code_execution_result` (and the programmatic
//!   `code_execution_result` / `encrypted_code_execution_result`) lists the
//!   files left in the command's `$OUTPUT_DIR` as `content[].file_id`. For
//!   each: `GET {base}/files/{id}` (metadata: `filename`, `mime_type`,
//!   `size_bytes`, `downloadable`), then `GET {base}/files/{id}/content`, with
//!   the request's key and `anthropic-version`. The Files API is out of beta
//!   (no beta header; the old `files-api-2025-04-14` would return the old
//!   shapes) and free (about 500 requests a minute per organization).
//! - Images (JPEG, PNG, GIF, WebP) become [`StreamEvent::Image`], anything
//!   else [`StreamEvent::File`] with its file name; the tool call's output
//!   gets each file's metadata object as `content[i].file` (the result block
//!   itself stays verbatim in the native payload). Files over
//!   [`MAX_FILE_BYTES`], files that are not downloadable and failed downloads
//!   (after retrying rate limits, 5xx and network errors) are notices, never
//!   failures of the answer.
//! - Nothing is deleted: Anthropic keeps code-execution outputs in the
//!   workspace until they are deleted. Replaying the turn needs nothing more
//!   (the result block with its `file_id`s goes back verbatim).
//! - The calls are logged at the `raw` target like the rest of the traffic,
//!   but not through the stream's observer: they run while the stream goes on
//!   (the observer's record of the stream's call would get mixed up), and
//!   `/info` lists the stream's calls only.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use omni_core::content::ToolCall;
use serde_json::Value;
use tokio::task::JoinHandle;

use crate::event::StreamEvent;

/// Largest file the bot fetches (the bot's default limit for files it takes).
pub const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
/// Time for one Files API request (metadata or the whole content).
const TIMEOUT: Duration = Duration::from_secs(60);
/// Time for all files of one tool result (what follows them waits, D30).
const BUDGET: Duration = Duration::from_secs(180);
/// Attempts per request (rate limits, 5xx, network errors are retried).
const ATTEMPTS: u32 = 3;

/// Image types shown as images (the rest are documents).
const IMAGE_TYPES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];

/// Fetches the files of tool results (one task per result).
pub struct Fetcher {
    http: reqwest::Client,
    /// API base (`https://api.anthropic.com/v1`).
    base: String,
    api_key: String,
    version: &'static str,
    max_bytes: u64,
    /// File ids already fetched (or being fetched) in this turn.
    seen: HashSet<String>,
}

impl Fetcher {
    pub fn new(http: reqwest::Client, base: &str, api_key: &str, version: &'static str) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            version,
            max_bytes: MAX_FILE_BYTES,
            seen: HashSet::new(),
        }
    }

    /// Start fetching the files a finished tool result names (none: `None`).
    /// The task's events go right after the result.
    pub fn start(&mut self, call: &ToolCall) -> Option<JoinHandle<Vec<StreamEvent>>> {
        if call.status.as_deref() != Some("completed") {
            return None;
        }
        let ids: Vec<String> =
            file_ids(call.output.as_ref()?).into_iter().filter(|id| self.seen.insert(id.clone())).collect();
        if ids.is_empty() {
            return None;
        }
        let job = Job {
            http: self.http.clone(),
            base: self.base.clone(),
            api_key: self.api_key.clone(),
            version: self.version,
            max_bytes: self.max_bytes,
            deadline: Instant::now() + BUDGET,
        };
        let call = call.clone();
        Some(tokio::spawn(async move { job.run(call, ids).await }))
    }
}

/// The `file_id`s of a tool result's output (`content[].file_id`), in order.
pub fn file_ids(output: &Value) -> Vec<String> {
    output
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("file_id").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}

/// The events of a finished fetch task (a task that panicked or was aborted: a notice).
pub fn joined(r: Result<Vec<StreamEvent>, tokio::task::JoinError>) -> Vec<StreamEvent> {
    r.unwrap_or_else(|e| {
        vec![StreamEvent::Notice(format!("the files that code execution wrote were not fetched: {e}"))]
    })
}

struct Job {
    http: reqwest::Client,
    base: String,
    api_key: String,
    version: &'static str,
    max_bytes: u64,
    deadline: Instant,
}

impl Job {
    /// Metadata and content of each file; the events: the tool call with the
    /// files' metadata in its output, then the images and files, then notices.
    async fn run(self, call: ToolCall, ids: Vec<String>) -> Vec<StreamEvent> {
        let mut media = Vec::new();
        let mut notices = Vec::new();
        let mut metas: Vec<(String, Value)> = Vec::new();
        for id in ids {
            let meta = match self.metadata(&id).await {
                Ok(m) => m,
                Err(e) => {
                    notices.push(not_fetched(&id, None, &e));
                    continue;
                }
            };
            metas.push((id.clone(), meta.clone()));
            let name = meta.get("filename").and_then(Value::as_str).filter(|n| !n.is_empty()).map(str::to_string);
            if meta.get("downloadable").and_then(Value::as_bool) == Some(false) {
                notices.push(not_fetched(&id, name.as_deref(), "the Files API says it is not downloadable"));
                continue;
            }
            if let Some(size) = meta.get("size_bytes").and_then(Value::as_u64)
                && size > self.max_bytes
            {
                let why = format!("{} is more than the bot's limit of {}", mb(size), mb(self.max_bytes));
                notices.push(not_fetched(&id, name.as_deref(), &why));
                continue;
            }
            match self.content(&id).await {
                Ok((data, header_mime)) => {
                    let mime = meta
                        .get("mime_type")
                        .and_then(Value::as_str)
                        .map(base_mime)
                        .filter(|m| !m.is_empty() && m != "application/octet-stream")
                        .or(header_mime)
                        .unwrap_or_else(|| sniff(&data).to_string());
                    if IMAGE_TYPES.contains(&mime.as_str()) {
                        media.push(StreamEvent::Image { data, mime });
                    } else {
                        media.push(StreamEvent::File { data, mime, name });
                    }
                }
                Err(e) => notices.push(not_fetched(&id, name.as_deref(), &e)),
            }
        }
        let mut out = Vec::new();
        if !metas.is_empty()
            && let Some(mut output) = call.output.clone()
        {
            if let Some(items) = output.get_mut("content").and_then(Value::as_array_mut) {
                for item in items {
                    let id = item.get("file_id").and_then(Value::as_str).map(str::to_string);
                    if let (Some(id), Some(o)) = (id, item.as_object_mut())
                        && let Some((_, m)) = metas.iter().find(|(x, _)| *x == id)
                    {
                        o.insert("file".into(), m.clone());
                    }
                }
            }
            out.push(StreamEvent::Tool(ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                status: None,
                input: None,
                output: Some(output),
            }));
        }
        out.extend(media);
        out.extend(notices);
        out
    }

    fn request(&self, url: &str, time: Duration) -> reqwest::RequestBuilder {
        self.http.get(url).header("x-api-key", &self.api_key).header("anthropic-version", self.version).timeout(time)
    }

    /// Time left for a request (none: `None`).
    fn time_left(&self) -> Option<Duration> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        (!left.is_zero()).then(|| left.min(TIMEOUT))
    }

    /// `GET /files/{id}`: the metadata object, verbatim.
    async fn metadata(&self, id: &str) -> Result<Value, String> {
        let url = format!("{}/files/{id}", self.base);
        let resp = self.send(&url).await?;
        let text = resp.text().await.map_err(|e| format!("reading the metadata failed: {e}"))?;
        tracing::trace!(target: "raw", body = %text, "upstream body");
        serde_json::from_str(&text).map_err(|e| format!("the metadata is not JSON ({e})"))
    }

    /// `GET /files/{id}/content`: the bytes and the response's content type.
    async fn content(&self, id: &str) -> Result<(Vec<u8>, Option<String>), String> {
        let url = format!("{}/files/{id}/content", self.base);
        let mut resp = self.send(&url).await?;
        if let Some(n) = resp.content_length()
            && n > self.max_bytes
        {
            return Err(format!("{} is more than the bot's limit of {}", mb(n), mb(self.max_bytes)));
        }
        let mime = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).map(base_mime);
        let mut data = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| format!("the download broke off: {e}"))? {
            data.extend_from_slice(&chunk);
            if data.len() as u64 > self.max_bytes {
                return Err(format!("it is more than the bot's limit of {}", mb(self.max_bytes)));
            }
        }
        tracing::trace!(target: "raw", body = %format!("({} bytes of {})", data.len(), mime.as_deref().unwrap_or("unknown type")), "upstream body");
        Ok((data, mime))
    }

    /// A GET with the API's headers; retried on rate limits, 5xx and network errors.
    async fn send(&self, url: &str) -> Result<reqwest::Response, String> {
        let mut last = String::new();
        for attempt in 1..=ATTEMPTS {
            let Some(time) = self.time_left() else {
                return Err(format!("the {} s the bot gives the files of one tool result ran out", BUDGET.as_secs()));
            };
            tracing::trace!(target: "raw", %url, body = "null", "upstream request");
            let (retry_in, error) = match self.request(url, time).send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let headers: Vec<String> = resp
                        .headers()
                        .iter()
                        .map(|(k, v)| format!("{k}: {}", String::from_utf8_lossy(v.as_bytes())))
                        .collect();
                    tracing::trace!(target: "raw", status, headers = ?headers, "upstream response");
                    if (200..300).contains(&status) {
                        return Ok(resp);
                    }
                    let (after, _) = crate::chatstream::retry_hints(resp.headers());
                    let body = resp.text().await.unwrap_or_default();
                    tracing::trace!(target: "raw", body = %body, "upstream body");
                    let retry = status == 429 || status >= 500;
                    (retry.then(|| after.unwrap_or(attempt as u64 * 2).min(10)), http_error(status, &body))
                }
                Err(e) => (Some(attempt as u64 * 2), format!("request failed: {e}")),
            };
            last = error;
            match retry_in {
                Some(secs) if attempt < ATTEMPTS => tokio::time::sleep(Duration::from_secs(secs)).await,
                _ => break,
            }
        }
        Err(last)
    }
}

/// `HTTP 404: File `file_…` not found.` from an error body (Anthropic's error object, or the body itself).
fn http_error(status: u16, body: &str) -> String {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| body.chars().take(300).collect());
    if message.trim().is_empty() { format!("HTTP {status}") } else { format!("HTTP {status}: {}", message.trim()) }
}

fn not_fetched(id: &str, name: Option<&str>, why: &str) -> StreamEvent {
    let what = match name {
        Some(n) => format!("{n} ({id})"),
        None => id.to_string(),
    };
    StreamEvent::Notice(format!("the file {what} that code execution wrote was not downloaded: {why}"))
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// `image/png` from `image/PNG; charset=…`.
fn base_mime(m: &str) -> String {
    m.split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

/// The type of `data` by its first bytes, for a file without one.
fn sniff(data: &[u8]) -> &'static str {
    if data.starts_with(b"\x89PNG") {
        "image/png"
    } else if data.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if data.starts_with(b"GIF8") {
        "image/gif"
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        "image/webp"
    } else if data.starts_with(b"%PDF") {
        "application/pdf"
    } else {
        "application/octet-stream"
    }
}
