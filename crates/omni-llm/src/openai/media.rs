//! Shared plumbing of the platforms' media APIs, which take one prompt and
//! its input media instead of a conversation: OpenAI's Images API
//! ([`super::images`]) and xAI's Imagine image and video APIs
//! ([`crate::xai::imagine`]).
//!
//! - [`media_input`]: what such a request takes from the conversation. The
//!   prompt is the last user message; its images (attached, or of the message
//!   it replies to) are inputs; when it replies to a generated answer, that
//!   answer's images (or video) come first: a reply edits what the model made
//!   (the owner's rule, as for Seedream). Nothing else of the conversation is
//!   sent (these APIs have no conversation), which a note says when there was
//!   more; a system prompt or an assistant prefill is an error.
//! - [`Http`]: JSON calls with the bot's traffic logging, cancellation, the
//!   idle timeout and the platform's error mapping (HTTP error bodies kept
//!   verbatim, as in [`crate::chatstream`]).
//! - Decoding of generated media ([`decode_b64`], [`sniff_image`]).

use std::time::Duration;

use omni_core::BlobRef;
use omni_core::content::{Part, Role, Turn};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::{ErrorKind, LlmError, Retry};
use crate::openrouter::chat::ErrorMapper;
use crate::{BlobLoader, MediaClass, Observer, classify_mime, provider};

// ------------------------------------------------------------------ input

/// What a single-prompt media request takes from the conversation.
#[derive(Debug, Default, Clone)]
pub(crate) struct MediaInput {
    /// The text of the last user message (its text parts, blank-line separated).
    pub prompt: String,
    /// The generated images (not drafts) of the answer the message replies to.
    pub answer_images: Vec<BlobRef>,
    /// The generated video of the answer the message replies to.
    pub answer_video: Option<BlobRef>,
    /// Images of the message itself (attached, or of the message it replies to).
    pub own_images: Vec<BlobRef>,
    /// Videos of the message itself.
    pub own_videos: Vec<BlobRef>,
    /// Shown with the answer.
    pub notes: Vec<String>,
}

/// The prompt and input media of the conversation's last user message (see
/// the module docs). `via` names the API for messages.
pub(crate) fn media_input(history: &[Turn], model: &str, via: &str) -> Result<MediaInput, LlmError> {
    let Some((last, earlier)) = history.split_last() else {
        return Err(LlmError::request(format!("`{model}` needs a prompt")));
    };
    if history.iter().any(|t| t.role == Role::System) {
        return Err(LlmError::request(format!(
            "`{model}` takes no system prompt via {via}: one prompt per request, no conversation"
        )));
    }
    if last.role != Role::User {
        return Err(LlmError::request(format!(
            "`{model}` takes a prompt, not an assistant prefill, via {via} (the conversation must end with a user message)"
        )));
    }
    let mut m = MediaInput::default();
    let mut texts = Vec::new();
    for p in &last.parts {
        match p {
            Part::Text { text, .. } => {
                if !text.trim().is_empty() {
                    texts.push(text.as_str());
                }
            }
            Part::Image { blob, .. } => m.own_images.push(blob.clone()),
            Part::File { blob, filename, .. } => match classify_mime(&blob.mime, filename.as_deref()) {
                MediaClass::Image => m.own_images.push(blob.clone()),
                MediaClass::Video => m.own_videos.push(blob.clone()),
                _ => {
                    let name = filename.clone().unwrap_or_else(|| blob.mime.clone());
                    return Err(LlmError::request(format!(
                        "`{model}` takes no {} files via {via} ({name}): only a prompt and images",
                        blob.mime
                    )));
                }
            },
            Part::RemoteMedia { url, .. } => {
                return Err(LlmError::request(format!("`{model}` cannot read remote media via {via} ({url})")));
            }
            Part::Location { .. } => return Err(provider::no_location(model, via)),
            Part::Reasoning(_) | Part::ToolCall(_) | Part::Refusal { .. } => {}
        }
    }
    m.prompt = texts.join("\n\n");
    // The answer this message replies to: what it generated is edited.
    if let Some(prev) = earlier.last().filter(|t| t.role == Role::Assistant) {
        for p in &prev.parts {
            match p {
                Part::Image { blob, generated: true, thought: false, .. } => m.answer_images.push(blob.clone()),
                Part::File { blob, filename, generated: true }
                    if classify_mime(&blob.mime, filename.as_deref()) == MediaClass::Video =>
                {
                    m.answer_video = Some(blob.clone());
                }
                _ => {}
            }
        }
    }
    if !earlier.is_empty() && m.answer_images.is_empty() && m.answer_video.is_none() {
        m.notes.push(format!(
            "ℹ️ {via} takes one prompt: only this message (and its images) is sent, not the earlier turns of the conversation"
        ));
    }
    Ok(m)
}

/// `image/png` from `image/PNG; charset=…`.
pub(crate) fn base_mime(mime: &str) -> String {
    mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

/// Check an input image's type against `allowed` (base MIME types) and load
/// it as a data URL; `max_url`: the API's limit on the data URL's length.
pub(crate) async fn image_data_url(
    blob: &BlobRef,
    allowed: &[&str],
    max_url: Option<usize>,
    model: &str,
    via: &str,
    blobs: &dyn BlobLoader,
) -> Result<String, LlmError> {
    let mime = base_mime(&blob.mime);
    let mime = if mime == "image/jpg" { "image/jpeg".to_string() } else { mime };
    if !allowed.contains(&mime.as_str()) {
        return Err(LlmError::request(format!(
            "`{model}` does not take {} images via {via}; formats: {}",
            blob.mime,
            allowed.iter().map(|m| m.trim_start_matches("image/")).collect::<Vec<_>>().join(", ")
        )));
    }
    data_url_checked(blob, &mime, max_url, model, via, blobs).await
}

/// Load a blob as a data URL of type `mime`, within `max_url` characters.
pub(crate) async fn data_url_checked(
    blob: &BlobRef,
    mime: &str,
    max_url: Option<usize>,
    model: &str,
    via: &str,
    blobs: &dyn BlobLoader,
) -> Result<String, LlmError> {
    let len = format!("data:{mime};base64,").len() + (blob.size as usize).div_ceil(3) * 4;
    if let Some(max) = max_url
        && len > max
    {
        return Err(LlmError::request(format!(
            "an input of `{model}` is too large for {via} ({} bytes: its base64 data URL would be {len} characters, the limit is {max})",
            blob.size
        )));
    }
    let bytes = provider::load(blobs, blob).await?;
    Ok(provider::data_url(mime, &bytes))
}

// ------------------------------------------------------------------ output

/// Standard base64 (as the APIs return it), tolerating surrounding whitespace.
pub(crate) fn decode_b64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s.trim()).ok()
}

/// The image type of `data` by its magic bytes, else `fallback`.
pub(crate) fn sniff_image(data: &[u8], fallback: &str) -> String {
    let t = if data.starts_with(b"\x89PNG") {
        "image/png"
    } else if data.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        "image/webp"
    } else if data.starts_with(b"GIF8") {
        "image/gif"
    } else {
        fallback
    };
    t.to_string()
}

/// `value` with every base64 string field named `key` replaced by a short
/// note: generated media are stored as the turn's images and files, the
/// native payload keeps only what is not already stored.
pub(crate) fn without_media(value: &Value, key: &str) -> Value {
    match value {
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| match v {
                    Value::String(s) if k == key && !s.is_empty() => {
                        (k.clone(), json!(format!("({} base64 characters: stored as the turn's media)", s.len())))
                    }
                    other => (k.clone(), without_media(other, key)),
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|v| without_media(v, key)).collect()),
        other => other.clone(),
    }
}

// ------------------------------------------------------------------ calls

/// A JSON reply: the body and the request id header (`x-request-id`).
#[derive(Debug, Clone)]
pub(crate) struct Reply {
    pub body: Value,
    pub request_id: Option<String>,
}

/// JSON calls to a platform's API (bearer auth), logged like the streams.
pub(crate) struct Http<'a> {
    pub client: &'a reqwest::Client,
    pub api_key: &'a str,
    pub idle_timeout: Duration,
    pub map_error: ErrorMapper,
    /// Final touch on HTTP errors (xAI: the error code from the body).
    pub refine: fn(LlmError) -> LlmError,
    pub observer: &'a dyn Observer,
    pub cancel: &'a CancellationToken,
}

/// Wait `d` unless cancelled first (then `Err`).
pub(crate) async fn pause(d: Duration, cancel: &CancellationToken) -> Result<(), LlmError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(LlmError::cancelled()),
        _ = tokio::time::sleep(d) => Ok(()),
    }
}

impl Http<'_> {
    pub async fn post(&self, url: &str, body: &Value) -> Result<Reply, LlmError> {
        self.observer.request(url, body);
        self.send(self.client.post(url).bearer_auth(self.api_key).json(body)).await
    }

    pub async fn get(&self, url: &str) -> Result<Reply, LlmError> {
        self.observer.request(url, &Value::Null);
        self.send(self.client.get(url).bearer_auth(self.api_key)).await
    }

    /// `GET url?query` (pairs URL-encoded; logged with the query).
    pub async fn get_query(&self, url: &str, query: &[(&str, &str)]) -> Result<Reply, LlmError> {
        if query.is_empty() {
            return self.get(url).await;
        }
        let full = reqwest::Url::parse_with_params(url, query)
            .map_err(|e| LlmError::request(format!("bad URL {url}: {e}")))?;
        self.observer.request(full.as_str(), &Value::Null);
        self.send(self.client.get(full).bearer_auth(self.api_key)).await
    }

    async fn respond(&self, rb: reqwest::RequestBuilder) -> Result<reqwest::Response, LlmError> {
        tokio::select! {
            _ = self.cancel.cancelled() => Err(LlmError::cancelled()),
            r = tokio::time::timeout(self.idle_timeout, rb.send()) => match r {
                Err(_) => Err(LlmError::new(ErrorKind::Timeout, Retry::Backoff, "no response within the idle timeout")),
                Ok(Err(e)) => Err(LlmError::network(format!("request failed: {e}"))),
                Ok(Ok(r)) => Ok(r),
            }
        }
    }

    fn headers(&self, resp: &reqwest::Response) -> (u16, Option<String>, Option<u64>) {
        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).to_string()))
            .collect();
        self.observer.response_status(status, &headers);
        let header = |h: &str| resp.headers().get(h).and_then(|v| v.to_str().ok()).map(str::to_string);
        let retry_after = header("retry-after").and_then(|s| s.trim().parse().ok());
        (status, header("x-request-id"), retry_after)
    }

    async fn send(&self, rb: reqwest::RequestBuilder) -> Result<Reply, LlmError> {
        let resp = self.respond(rb).await?;
        let (status, request_id, retry_after) = self.headers(&resp);
        let text = tokio::select! {
            _ = self.cancel.cancelled() => return Err(LlmError::cancelled()),
            t = tokio::time::timeout(self.idle_timeout, resp.text()) => match t {
                Err(_) => return Err(LlmError::new(ErrorKind::Timeout, Retry::Backoff, "response body not complete within the idle timeout")),
                Ok(Err(e)) => return Err(LlmError::network(format!("response read error: {e}"))),
                Ok(Ok(t)) => t,
            }
        };
        if !(200..300).contains(&status) {
            self.observer.body_text(&text);
            let e = crate::chatstream::http_error(status, retry_after, &text, self.map_error);
            return Err((self.refine)(e));
        }
        // Logged as data: the bot's logger stores the large base64 media of a JSON body as blobs.
        self.observer.sse_data(&text);
        let body = serde_json::from_str(&text).map_err(|e| {
            LlmError::new(ErrorKind::Decode, Retry::Backoff, format!("bad JSON response: {e}"))
                .with_details(Value::String(text.chars().take(4000).collect()))
        })?;
        Ok(Reply { body, request_id })
    }

    /// Download a generated file from the (temporary, signed) URL the API
    /// returned: no credentials are sent to it. Returns the bytes and the
    /// `content-type`.
    pub async fn download(&self, url: &str) -> Result<(Vec<u8>, Option<String>), LlmError> {
        self.observer.request(url, &Value::Null);
        let resp = self.respond(self.client.get(url)).await?;
        let (status, _, retry_after) = self.headers(&resp);
        let mime = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).map(base_mime);
        let bytes = tokio::select! {
            _ = self.cancel.cancelled() => return Err(LlmError::cancelled()),
            b = tokio::time::timeout(self.idle_timeout, resp.bytes()) => match b {
                Err(_) => return Err(LlmError::new(ErrorKind::Timeout, Retry::Backoff, "download not complete within the idle timeout")),
                Ok(Err(e)) => return Err(LlmError::network(format!("download failed: {e}"))),
                Ok(Ok(b)) => b,
            }
        };
        if !(200..300).contains(&status) {
            let text = String::from_utf8_lossy(&bytes).chars().take(4000).collect::<String>();
            self.observer.body_text(&text);
            let mut e = LlmError::new(
                ErrorKind::Http,
                crate::error::retry_for_status(status, retry_after),
                format!("downloading the generated file failed: HTTP {status}"),
            );
            e.status = Some(status);
            e.details = Some(json!({"url": url, "body": text}));
            return Err(e);
        }
        self.observer.body_text(&format!("({} bytes of {})", bytes.len(), mime.as_deref().unwrap_or("unknown type")));
        Ok((bytes.to_vec(), mime))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::openai::wire::tests::{MemBlobs, file, photo, user};
    use omni_core::Origin;

    /// The requests a test server got: `METHOD path`, authorization, JSON body.
    pub(crate) type Requests = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>, Value)>>>;

    /// A local HTTP server answering requests in order, one per connection
    /// (`connection: close`), with `(status, content type, body)`; returns
    /// its base URL and the requests it got.
    pub(crate) async fn serve_seq(replies: Vec<(u16, &'static str, Vec<u8>)>) -> (String, Requests) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let got: Requests = Default::default();
        let log = got.clone();
        tokio::spawn(async move {
            for (status, ctype, body) in replies {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 65536];
                let (head, req_body) = loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break (String::new(), Value::Null);
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                        let len: usize = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        while buf.len() < pos + 4 + len {
                            let n = sock.read(&mut tmp).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            buf.extend_from_slice(&tmp[..n]);
                        }
                        let end = (pos + 4 + len).min(buf.len());
                        break (head, serde_json::from_slice(&buf[pos + 4..end]).unwrap_or(Value::Null));
                    }
                };
                let first = head.lines().next().unwrap_or("");
                let mut it = first.split(' ');
                let line = format!("{} {}", it.next().unwrap_or(""), it.next().unwrap_or(""));
                let auth = head.lines().find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("authorization").then(|| v.trim().to_string())
                });
                log.lock().unwrap().push((line, auth, req_body));
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nx-request-id: req_{}\r\nconnection: close\r\n\r\n",
                    body.len(),
                    log.lock().unwrap().len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{addr}/v1"), got)
    }

    fn generated(blob: &BlobRef) -> Part {
        Part::Image { blob: blob.clone(), filename: None, generated: true, dims: None, thought: false }
    }

    #[test]
    fn a_reply_edits_what_the_answer_generated() {
        let (img, (b_in, _)) = photo("image/png", b"\x89PNG-own");
        let out = BlobRef::for_bytes(b"\x89PNG-out", "image/png");
        let draft = BlobRef::for_bytes(b"\x89PNG-draft", "image/png");
        let video = BlobRef::for_bytes(b"....ftypmp42", "video/mp4");
        let answer = Turn {
            role: Role::Assistant,
            origin: Origin::Live,
            parts: vec![
                Part::Image { blob: draft, filename: None, generated: true, dims: None, thought: true },
                generated(&out),
                Part::File { blob: video.clone(), filename: Some("v.mp4".into()), generated: true },
            ],
            native: None,
        };
        let history = vec![Turn::user_text("a cat"), answer, user(vec![Part::text("make it blue"), img])];
        let m = media_input(&history, "m", "the API").unwrap();
        assert_eq!(m.prompt, "make it blue");
        // The answer's final image (not the draft), and the message's own.
        assert_eq!((m.answer_images, m.own_images), (vec![out], vec![b_in]));
        assert_eq!(m.answer_video, Some(video));
        assert!(m.notes.is_empty(), "an edit chain needs no note: {:?}", m.notes);
    }

    #[test]
    fn what_cannot_be_sent_is_said_or_refused() {
        // Earlier turns that are not an edit chain: a note.
        let history = vec![Turn::user_text("q"), Turn::assistant_text("text answer"), Turn::user_text("draw it")];
        let m = media_input(&history, "m", "the Images API").unwrap();
        assert_eq!(m.notes.len(), 1);
        assert!(m.notes[0].contains("not the earlier turns"), "{}", m.notes[0]);
        // A system prompt or a prefill: errors.
        let sys =
            Turn { role: Role::System, origin: Origin::Synthetic, parts: vec![Part::text("be nice")], native: None };
        assert!(media_input(&[sys, Turn::user_text("x")], "m", "v").unwrap_err().message.contains("no system prompt"));
        let e = media_input(&[Turn::user_text("x"), Turn::assistant_text("y")], "m", "v").unwrap_err();
        assert!(e.message.contains("not an assistant prefill"), "{}", e.message);
        // Documents, locations, remote media.
        let (pdf, _) = file("a.pdf", "application/pdf", b"%PDF");
        assert!(media_input(&[user(vec![pdf])], "m", "v").unwrap_err().message.contains("takes no application/pdf"));
        let loc = Part::Location { latitude: 1.0, longitude: 2.0, accuracy_m: None, title: None, address: None };
        assert!(media_input(&[user(vec![loc])], "m", "v").unwrap_err().message.contains("location"));
        // Videos and image files are collected.
        let (mp4, _) = file("clip.mp4", "video/mp4", b"....");
        let (png, _) = file("p.png", "image/png", b"\x89PNG");
        let m = media_input(&[user(vec![Part::text("a"), Part::text("b"), mp4, png])], "m", "v").unwrap();
        assert_eq!((m.prompt.as_str(), m.own_videos.len(), m.own_images.len()), ("a\n\nb", 1, 1));
    }

    #[tokio::test]
    async fn input_images_are_checked_and_sized() {
        let (_, (b, data)) = photo("image/jpg", b"\xff\xd8\xff-jpeg");
        let blobs = MemBlobs(vec![(b.clone(), data)]);
        let url = image_data_url(&b, &["image/png", "image/jpeg"], None, "m", "v", &blobs).await.unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"), "{url}");
        let e = image_data_url(&b, &["image/png"], None, "m", "v", &blobs).await.unwrap_err();
        assert!(e.message.contains("formats: png"), "{}", e.message);
        let e = image_data_url(&b, &["image/jpeg"], Some(20), "m", "v", &blobs).await.unwrap_err();
        assert!(e.message.contains("too large"), "{}", e.message);
    }

    #[test]
    fn media_bytes_are_recognized_and_kept_out_of_natives() {
        assert_eq!(sniff_image(b"\x89PNG\r\n", "x"), "image/png");
        assert_eq!(sniff_image(b"\xff\xd8\xff\xe0", "x"), "image/jpeg");
        assert_eq!(sniff_image(b"RIFF\0\0\0\0WEBPVP8 ", "x"), "image/webp");
        assert_eq!(sniff_image(b"????", "image/png"), "image/png");
        assert_eq!(decode_b64(" aGk= "), Some(b"hi".to_vec()));
        let big = "A".repeat(100);
        let v = json!({"data": [{"b64_json": big, "mime_type": "image/png"}], "usage": {"b64": "kept"}});
        let s = without_media(&v, "b64_json");
        assert_eq!(s["data"][0]["b64_json"], "(100 base64 characters: stored as the turn's media)");
        assert_eq!(s["data"][0]["mime_type"], "image/png");
        assert_eq!(s["usage"]["b64"], "kept");
    }
}
