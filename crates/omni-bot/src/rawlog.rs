//! Verbose logging of raw upstream traffic (target `raw`, TRACE level).
//!
//! Each line is emitted inside the conversation span, so it carries the
//! bot/chat/user/turn/attempt prefix. Large binary payloads (data URLs, long
//! base64 strings such as images or encrypted reasoning) are stored in the
//! blob store and replaced by `<blob sha256:… N bytes mime>` references:
//! nothing is lost, logs stay readable.

use omni_core::json::redact_blobs;
use omni_llm::{BlobLoader, BoxFuture, Observer};
use serde_json::Value;
use tracing::{Level, enabled, trace};

use crate::store::blobs::BlobStore;

pub struct RawLog {
    pub blobs: BlobStore,
}

impl RawLog {
    fn redact(&self, v: &Value) -> Value {
        redact_blobs(v, &mut |bytes, mime| match self.blobs.put_sync(bytes, mime) {
            Ok(r) => r,
            Err(_) => omni_core::BlobRef::for_bytes(bytes, mime),
        })
    }
}

impl Observer for RawLog {
    fn request(&self, url: &str, body: &Value) {
        if enabled!(target: "raw", Level::TRACE) {
            trace!(target: "raw", %url, body = %self.redact(body), "upstream request");
        }
    }
    fn response_status(&self, status: u16, headers: &[(String, String)]) {
        if enabled!(target: "raw", Level::TRACE) {
            let h: Vec<String> = headers.iter().map(|(k, v)| format!("{k}: {v}")).collect();
            trace!(target: "raw", status, headers = ?h, "upstream response");
        }
    }
    fn sse_comment(&self, comment: &str) {
        trace!(target: "raw", comment, "sse comment");
    }
    fn sse_data(&self, data: &str) {
        if !enabled!(target: "raw", Level::TRACE) {
            return;
        }
        if data.len() > omni_core::json::BLOB_THRESHOLD
            && let Ok(v) = serde_json::from_str::<Value>(data)
        {
            trace!(target: "raw", data = %self.redact(&v), "sse data");
            return;
        }
        trace!(target: "raw", data, "sse data");
    }
    fn body_text(&self, text: &str) {
        trace!(target: "raw", body = text, "upstream body");
    }
}

pub struct Blobs(pub BlobStore);

impl BlobLoader for Blobs {
    fn load<'a>(&'a self, r: &'a omni_core::BlobRef) -> BoxFuture<'a, anyhow::Result<Vec<u8>>> {
        Box::pin(async move { self.0.get(r).await })
    }
}
