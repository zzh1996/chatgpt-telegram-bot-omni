//! Files OpenAI's code interpreter made, fetched while the call streams and
//! shown where the stream cites them.
//!
//! - `container_file_citation` annotations name a file the code wrote in its
//!   container. The container is discarded about 20 minutes after its last
//!   use (code interpreter guide: "download any files you may need from the
//!   container while it is active"), so the file is fetched as soon as it is
//!   cited: `GET {base}/containers/{container_id}/files/{file_id}` for its
//!   real name (the last part of its `path`; the citation's `filename` is
//!   often only `cfile_….png`), then `…/content` for the bytes. Images become
//!   [`StreamEvent::Image`], except one whose bytes equal an image the code
//!   outputs already showed (the `plt.show()` plot is both an inline output
//!   and a container file); other files become [`StreamEvent::File`]; a
//!   failure is a notice naming the file.
//! - A code output image given as a URL instead of inline data is fetched
//!   the same way (with the API key only when it is on the API's own host).
//!
//! [`Gate`] keeps the order (D30): what the stream produces after a fetch
//! waits until the fetch ends, so the file appears where it was cited, while
//! the stream itself goes on being read. A fetch done once is remembered: a
//! file cited again, or the same response replayed from OpenAI's stored copy,
//! is not fetched twice. The fetches do not go through the call's
//! [`crate::Observer`] (a request there would open a new call in the
//! attempt's metadata in the middle of the stream); their traffic is logged
//! at TRACE (target `raw`) when the result is shown.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::EventSink;
use crate::event::StreamEvent;

/// Something the stream cites that has to be fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fetch {
    /// A file the code wrote in its container (`container_file_citation`).
    ContainerFile { container_id: String, file_id: String, filename: String },
    /// A code output image given by URL.
    Image { url: String },
}

impl Fetch {
    fn key(&self) -> String {
        match self {
            Fetch::ContainerFile { container_id, file_id, .. } => format!("{container_id}/{file_id}"),
            Fetch::Image { url } => url.clone(),
        }
    }
}

/// What the parser hands over in stream order when a fetch is among it.
pub(crate) enum Queued {
    Event(StreamEvent),
    Fetch(Fetch),
}

/// One HTTP exchange of a fetch, for the raw log.
#[derive(Debug, Clone)]
struct Exchange {
    url: String,
    status: Option<u16>,
    headers: Vec<(String, String)>,
    /// The JSON body, or what the bytes were, or the error.
    body: String,
}

/// A finished fetch.
#[derive(Debug, Clone)]
struct Fetched {
    fetch: Fetch,
    /// The file name (the last part of its container path, else the citation's).
    name: String,
    /// The bytes and their MIME type, or why there are none.
    result: Result<(Vec<u8>, String), String>,
    log: Vec<Exchange>,
}

impl Fetched {
    /// What to show: the image or the file, or a notice.
    fn events(&self, shown: &HashSet<String>) -> Vec<StreamEvent> {
        match (&self.result, &self.fetch) {
            (Ok((data, mime)), fetch) => {
                let image = super::media::sniff_image(data, "");
                if !image.is_empty() {
                    // The plot `plt.show()` also returned inline: shown already.
                    if shown.contains(&omni_core::blob::sha256_hex(data)) {
                        return Vec::new();
                    }
                    return vec![StreamEvent::Image { data: data.clone(), mime: image }];
                }
                match fetch {
                    Fetch::ContainerFile { .. } => {
                        vec![StreamEvent::File {
                            data: data.clone(),
                            mime: mime.clone(),
                            name: Some(self.name.clone()),
                        }]
                    }
                    Fetch::Image { url } => {
                        vec![StreamEvent::Notice(format!("📎 the code output image {url} is not an image ({mime})"))]
                    }
                }
            }
            (Err(why), Fetch::ContainerFile { .. }) => vec![StreamEvent::Notice(format!(
                "📎 could not download {} from the code interpreter's container: {why}",
                self.name
            ))],
            (Err(why), Fetch::Image { url }) => {
                vec![StreamEvent::Notice(format!("📎 could not download the code output image {url}: {why}"))]
            }
        }
    }

    fn log(&self) {
        for x in &self.log {
            tracing::trace!(target: "raw", url = %x.url, "upstream request");
            if let Some(status) = x.status {
                let h: Vec<String> = x.headers.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                tracing::trace!(target: "raw", status, headers = ?h, "upstream response");
            }
            tracing::trace!(target: "raw", body = %x.body, "upstream body");
        }
    }
}

enum Slot {
    Event(StreamEvent),
    Running(JoinHandle<Fetched>),
    Ready(Fetched),
}

#[derive(Default)]
struct State {
    /// What waits to be emitted, in stream order (only while a fetch is ahead).
    queue: VecDeque<Slot>,
    /// Finished fetches by key.
    done: HashMap<String, Fetched>,
    /// SHA-256 of the images the code outputs showed.
    shown: HashSet<String>,
}

struct Inner {
    client: reqwest::Client,
    api_key: String,
    /// `https://api.openai.com/v1`.
    base: String,
    timeout: Duration,
    state: Mutex<State>,
}

/// Starts the fetches (for the parser) and keeps their place in the stream
/// (with [`Gate`]).
#[derive(Clone)]
pub(crate) struct Fetcher(Arc<Inner>);

impl Fetcher {
    /// `base`: the API's base URL; `timeout`: per request.
    pub fn new(client: reqwest::Client, api_key: &str, base: &str, timeout: Duration) -> Self {
        Fetcher(Arc::new(Inner {
            client,
            api_key: api_key.to_string(),
            base: base.trim_end_matches('/').to_string(),
            timeout,
            state: Mutex::new(State::default()),
        }))
    }

    /// Events and fetches, in stream order: the fetches start now, their
    /// results are shown at their place.
    pub fn enqueue(&self, items: Vec<Queued>) {
        let mut st = self.0.state.lock();
        for item in items {
            let slot = match item {
                Queued::Event(e) => Slot::Event(e),
                Queued::Fetch(what) => match st.done.get(&what.key()) {
                    Some(f) => Slot::Ready(f.clone()),
                    None => {
                        let inner = self.0.clone();
                        Slot::Running(tokio::spawn(async move { fetch(&inner, what).await }))
                    }
                },
            };
            st.queue.push_back(slot);
        }
    }

    /// An image a code output showed (a container file with the same bytes is not shown again).
    pub fn shown(&self, data: &[u8]) {
        self.0.state.lock().shown.insert(omni_core::blob::sha256_hex(data));
    }

    /// Something waits to be emitted.
    fn busy(&self) -> bool {
        !self.0.state.lock().queue.is_empty()
    }

    fn push(&self, ev: StreamEvent) {
        self.0.state.lock().queue.push_back(Slot::Event(ev));
    }

    /// The next slot, if it can be emitted now.
    fn next_ready(&self) -> Option<Slot> {
        let mut st = self.0.state.lock();
        match st.queue.front() {
            Some(Slot::Running(h)) if !h.is_finished() => None,
            Some(_) => st.queue.pop_front(),
            None => None,
        }
    }

    fn next(&self) -> Option<Slot> {
        self.0.state.lock().queue.pop_front()
    }

    /// The events of a finished fetch (remembered for later citations).
    fn finished(&self, f: Fetched) -> Vec<StreamEvent> {
        f.log();
        let mut st = self.0.state.lock();
        let evs = f.events(&st.shown);
        st.done.insert(f.fetch.key(), f);
        evs
    }

    fn abort_all(&self) {
        for s in self.0.state.lock().queue.drain(..) {
            if let Slot::Running(h) = s {
                h.abort();
            }
        }
    }
}

/// A slot ready to be emitted.
enum Emit {
    Event(StreamEvent),
    Fetched(Fetched),
    /// A fetch task that did not end normally.
    Failed(String),
}

fn joined(r: Result<Fetched, tokio::task::JoinError>) -> Emit {
    match r {
        Ok(f) => Emit::Fetched(f),
        Err(e) => Emit::Failed(format!("the download task failed: {e}")),
    }
}

impl Slot {
    /// The slot's content (a running fetch has finished).
    fn now(self) -> Emit {
        match self {
            Slot::Event(e) => Emit::Event(e),
            Slot::Ready(f) => Emit::Fetched(f),
            Slot::Running(h) => match h.now_or_never() {
                Some(r) => joined(r),
                None => Emit::Failed("the download did not finish".into()),
            },
        }
    }
}

/// The sink a stream writes to: events pass straight through, except behind a
/// fetch still running, where they wait (in order) until it ends.
pub(crate) struct Gate<'a> {
    sink: &'a mut dyn EventSink,
    files: Option<Fetcher>,
}

impl<'a> Gate<'a> {
    pub fn new(sink: &'a mut dyn EventSink, files: Option<Fetcher>) -> Self {
        Gate { sink, files }
    }

    fn emit(&mut self, f: &Fetcher, e: Emit) {
        match e {
            Emit::Event(e) => self.sink.event(e),
            Emit::Fetched(x) => {
                for e in f.finished(x) {
                    self.sink.event(e);
                }
            }
            Emit::Failed(why) => {
                self.sink.event(StreamEvent::Notice(format!("📎 could not download a cited file: {why}")))
            }
        }
    }

    /// Emit what can be emitted now.
    fn drain(&mut self) {
        let Some(f) = self.files.clone() else { return };
        while let Some(slot) = f.next_ready() {
            self.emit(&f, slot.now());
        }
    }

    /// Wait for the fetches still running and emit everything held back (at
    /// the end of a stream). Cancelled: the rest is dropped.
    pub async fn flush(&mut self, cancel: &CancellationToken) {
        let Some(f) = self.files.clone() else { return };
        while let Some(slot) = f.next() {
            let e = match slot {
                Slot::Running(h) => {
                    let abort = h.abort_handle();
                    tokio::select! {
                        _ = cancel.cancelled() => {
                            abort.abort();
                            f.abort_all();
                            return;
                        }
                        r = h => joined(r),
                    }
                }
                other => other.now(),
            };
            self.emit(&f, e);
        }
    }
}

impl EventSink for Gate<'_> {
    fn event(&mut self, ev: StreamEvent) {
        match &self.files {
            Some(f) if f.busy() => {
                f.push(ev);
                self.drain();
            }
            _ => self.sink.event(ev),
        }
    }
}

// ------------------------------------------------------------------ fetching

/// A file's MIME type: the response's own type unless generic, else by the
/// name's extension (OpenAI's file types, which cover what the code
/// interpreter writes), else `application/octet-stream`.
fn file_mime(name: &str, header: Option<&str>) -> String {
    let declared = header
        .map(super::media::base_mime)
        .filter(|m| !m.is_empty() && !matches!(m.as_str(), "application/octet-stream" | "binary/octet-stream"));
    if let Some(m) = declared {
        return m;
    }
    if let Some(m) = super::wire::openai_file_mime("application/octet-stream", Some(name)) {
        return m;
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" | "tgz" => "application/gzip",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// `GET url` (with the API key when `auth`): the body and its content type.
async fn get(
    inner: &Inner,
    url: &str,
    auth: bool,
    log: &mut Vec<Exchange>,
) -> Result<(Vec<u8>, Option<String>), String> {
    let mut rb = inner.client.get(url);
    if auth {
        rb = rb.bearer_auth(&inner.api_key);
    }
    let mut x = Exchange { url: url.to_string(), status: None, headers: Vec::new(), body: String::new() };
    let sent = tokio::time::timeout(inner.timeout, async {
        let resp = rb.send().await.map_err(|e| format!("request failed: {e}"))?;
        x.status = Some(resp.status().as_u16());
        x.headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).to_string()))
            .collect();
        let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);
        let bytes = resp.bytes().await.map_err(|e| format!("download failed: {e}"))?;
        Ok::<_, String>((bytes.to_vec(), ctype))
    })
    .await;
    let out = match sent {
        Err(_) => Err(format!("no complete response within {}s", inner.timeout.as_secs())),
        Ok(Err(e)) => Err(e),
        Ok(Ok((bytes, ctype))) => match x.status {
            Some(s) if !(200..300).contains(&s) => {
                let text: String = String::from_utf8_lossy(&bytes).chars().take(500).collect();
                x.body = text.clone();
                Err(format!("HTTP {s} {text}"))
            }
            _ => {
                x.body = if ctype.as_deref().is_some_and(|c| c.contains("json")) {
                    String::from_utf8_lossy(&bytes).chars().take(2000).collect()
                } else {
                    format!("({} bytes of {})", bytes.len(), ctype.as_deref().unwrap_or("unknown type"))
                };
                Ok((bytes, ctype))
            }
        },
    };
    if let Err(e) = &out
        && x.body.is_empty()
    {
        x.body = e.clone();
    }
    log.push(x);
    out
}

async fn fetch(inner: &Inner, what: Fetch) -> Fetched {
    let mut log = Vec::new();
    match &what {
        Fetch::ContainerFile { container_id, file_id, filename } => {
            let url = format!("{}/containers/{container_id}/files/{file_id}", inner.base);
            // The real name: the last part of the file's path in the container.
            let name = match get(inner, &url, true, &mut log).await {
                Ok((body, _)) => serde_json::from_slice::<Value>(&body)
                    .ok()
                    .and_then(|v| v.get("path").and_then(Value::as_str).map(str::to_string))
                    .and_then(|p| p.rsplit('/').next().filter(|n| !n.is_empty()).map(str::to_string)),
                Err(_) => None,
            }
            .unwrap_or_else(|| filename.clone());
            let result = get(inner, &format!("{url}/content"), true, &mut log).await.map(|(data, ctype)| {
                let mime = file_mime(&name, ctype.as_deref());
                (data, mime)
            });
            Fetched { fetch: what, name, result, log }
        }
        Fetch::Image { url } => {
            // The API key only goes to the API itself.
            let own = url.starts_with(&format!("{}/", inner.base));
            let result = get(inner, url, own, &mut log).await.map(|(data, ctype)| {
                let declared = ctype.as_deref().map(super::media::base_mime).unwrap_or_default();
                let mime = super::media::sniff_image(&data, &declared);
                (data, mime)
            });
            let name = url.clone();
            Fetched { fetch: what, name, result, log }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_types_of_container_files() {
        assert_eq!(file_mime("data.csv", Some("application/octet-stream")), "text/csv");
        assert_eq!(file_mime("report.pdf", None), "application/pdf");
        assert_eq!(file_mime("a.txt", Some("text/plain; charset=utf-8")), "text/plain");
        assert_eq!(file_mime("archive.zip", None), "application/zip");
        assert_eq!(file_mime("model.pkl", None), "application/octet-stream");
    }

    #[test]
    fn an_image_shown_inline_is_not_shown_again() {
        let png = b"\x89PNG\r\n-plot".to_vec();
        let f = Fetched {
            fetch: Fetch::ContainerFile {
                container_id: "c".into(),
                file_id: "f".into(),
                filename: "cfile_f.png".into(),
            },
            name: "plot.png".into(),
            result: Ok((png.clone(), "image/png".into())),
            log: Vec::new(),
        };
        assert!(matches!(&f.events(&HashSet::new())[..], [StreamEvent::Image { mime, .. }] if mime == "image/png"));
        let shown: HashSet<String> = [omni_core::blob::sha256_hex(&png)].into();
        assert!(f.events(&shown).is_empty());
        // Other files are documents with their real name; failures are notices.
        let csv =
            Fetched { result: Ok((b"a,b\n1,2\n".to_vec(), "text/csv".into())), name: "data.csv".into(), ..f.clone() };
        assert!(matches!(&csv.events(&shown)[..],
            [StreamEvent::File { name: Some(n), mime, .. }] if n == "data.csv" && mime == "text/csv"));
        let failed = Fetched { result: Err("HTTP 404 {\"error\":\"expired\"}".into()), ..f };
        assert!(matches!(&failed.events(&shown)[..],
            [StreamEvent::Notice(n)] if n.contains("could not download plot.png") && n.contains("HTTP 404")));
    }
}
