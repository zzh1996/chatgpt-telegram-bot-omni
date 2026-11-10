//! OpenRouter client.

pub mod catalog;
pub mod chat;
pub mod options;

use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::error::{ErrorKind, LlmError, Retry};
use crate::event::StreamEvent;
use crate::sse::{SseItem, SseParser};
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, Observer, Prepared};

use catalog::{Catalog, EndpointInfo, ModelInfo};
use options::FidelityConfig;

#[derive(Debug, Clone)]
pub struct OpenRouterConfig {
    pub api_key: String,
    pub base_url: String,
    pub http_referer: Option<String>,
    pub x_title: Option<String>,
    pub connect_timeout: Duration,
    /// Max time without any bytes (including keep-alive comments) before giving up.
    pub idle_timeout: Duration,
    pub fidelity: FidelityConfig,
    /// Always request the upstream body echo (logged in verbose mode).
    pub echo_upstream_body: bool,
    /// Request router metadata (`X-OpenRouter-Metadata: enabled`).
    pub router_metadata: bool,
}

#[derive(Clone)]
pub struct OpenRouter {
    cfg: OpenRouterConfig,
    http: reqwest::Client,
    pub catalog: Catalog,
}

impl OpenRouter {
    pub fn new(cfg: OpenRouterConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(cfg.connect_timeout)
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()?;
        Ok(Self { cfg, http, catalog: Catalog::default() })
    }

    pub fn config(&self) -> &OpenRouterConfig {
        &self.cfg
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cfg.base_url.trim_end_matches('/'), path)
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut rb = rb.bearer_auth(&self.cfg.api_key);
        if let Some(r) = &self.cfg.http_referer {
            rb = rb.header("HTTP-Referer", r);
        }
        if let Some(t) = &self.cfg.x_title {
            rb = rb.header("X-OpenRouter-Title", t);
        }
        rb
    }

    /// Refresh the model catalog (all output modalities).
    pub async fn refresh_catalog(&self) -> anyhow::Result<usize> {
        let resp = self
            .auth(self.http.get(self.url("/models")).query(&[("output_modalities", "all")]))
            .timeout(Duration::from_secs(60))
            .send()
            .await?
            .error_for_status()?;
        let v: Value = resp.json().await?;
        let list = v.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut models = Vec::with_capacity(list.len());
        for m in list {
            match serde_json::from_value::<ModelInfo>(m) {
                Ok(m) => models.push(m),
                Err(e) => debug!("skipping unparsable model entry: {e}"),
            }
        }
        let n = models.len();
        self.catalog.replace(models);
        Ok(n)
    }

    pub async fn model_info(&self, id: &str) -> Option<std::sync::Arc<ModelInfo>> {
        if self.catalog.is_stale()
            && let Err(e) = self.refresh_catalog().await
        {
            warn!("model catalog refresh failed: {e:#}");
        }
        self.catalog.get(id)
    }

    pub async fn endpoints(&self, id: &str) -> anyhow::Result<std::sync::Arc<Vec<EndpointInfo>>> {
        if let Some(e) = self.catalog.endpoints(id) {
            return Ok(e);
        }
        let base = id.split(':').next().unwrap_or(id);
        let resp = self
            .auth(self.http.get(self.url(&format!("/models/{base}/endpoints"))))
            .timeout(Duration::from_secs(30))
            .send()
            .await?
            .error_for_status()?;
        let v: Value = resp.json().await?;
        let list = v.pointer("/data/endpoints").and_then(Value::as_array).cloned().unwrap_or_default();
        let eps: Vec<EndpointInfo> = list.into_iter().filter_map(|e| serde_json::from_value(e).ok()).collect();
        Ok(self.catalog.put_endpoints(id, eps))
    }

    /// `GET /generation?id=` (cost and metadata after the fact).
    pub async fn generation(&self, id: &str) -> anyhow::Result<Option<Value>> {
        let resp = self
            .auth(self.http.get(self.url("/generation")).query(&[("id", id)]))
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        let v: Value = resp.error_for_status()?.json().await?;
        Ok(v.get("data").cloned())
    }

    /// `GET /key` (credit usage / limits of the API key).
    pub async fn key_info(&self) -> anyhow::Result<Value> {
        let resp = self.auth(self.http.get(self.url("/key"))).timeout(Duration::from_secs(30)).send().await?;
        Ok(resp.error_for_status()?.json().await?)
    }

    /// Build a Chat Completions request.
    pub async fn prepare_chat(
        &self,
        input: &CallInput<'_>,
        config_presets: &[(String, Value)],
        blobs: &dyn BlobLoader,
    ) -> Result<Prepared, LlmError> {
        let info = self.model_info(input.model).await;
        let (messages, notes) = chat::convert_history(input.history, input.model, info.as_deref(), blobs).await?;
        let mut base = json!({"model": input.model, "messages": messages, "stream": true});
        if let Some(s) = &input.session_id {
            base["session_id"] = json!(s);
        }
        if let Some(u) = &input.user {
            base["user"] = json!(u);
        }
        let mut resolved =
            options::resolve(base, input.model, info.as_deref(), &self.cfg.fidelity, config_presets, input.options)
                .map_err(LlmError::request)?;
        if notes.needs_file_parser {
            // Force native file handling; detect router-side parsing via annotations.
            let plugins = resolved.body["plugins"].as_array().cloned().unwrap_or_default();
            let mut plugins: Vec<Value> = plugins.into_iter().filter(|p| p["id"] != "file-parser").collect();
            plugins.push(json!({"id": "file-parser", "pdf": {"engine": "native"}}));
            resolved.body["plugins"] = Value::Array(plugins);
            resolved.applied.push(options::Applied {
                source: "fidelity".into(),
                patch: json!({"plugins": [{"id": "file-parser", "pdf": {"engine": "native"}}]}),
            });
        }
        self.check_native_search(&mut resolved, input.model).await?;
        // Service tiers are separate endpoints (`openai/flex`); include them in the pin.
        if let (Some(tier), Some(only)) = (
            resolved.body.get("service_tier").and_then(Value::as_str).map(str::to_string),
            resolved.body.pointer("/provider/only").and_then(Value::as_array).cloned(),
        ) && !matches!(tier.as_str(), "auto" | "default")
        {
            let mut slugs = only.clone();
            for s in only.iter().filter_map(Value::as_str) {
                if !s.contains('/') {
                    slugs.push(json!(format!("{s}/{tier}")));
                }
            }
            resolved.body["provider"]["only"] = Value::Array(slugs);
        }
        if self.cfg.echo_upstream_body && resolved.body.get("debug").is_none() {
            resolved.body["debug"] = json!({"echo_upstream_body": true});
        }
        if info.is_none() {
            resolved.warnings.push(format!(
                "model `{}` is not in OpenRouter's catalog (yet); sending anyway without capability-based defaults",
                input.model
            ));
        }
        Ok(Prepared {
            dialect: chat::DIALECT,
            model: input.model.to_string(),
            url: self.url("/chat/completions"),
            body: resolved.body,
            applied: resolved.applied,
            warnings: resolved.warnings,
            notes: notes.notes,
            show_debug: resolved.show_debug,
        })
    }

    /// `engine: "native"` web search silently falls back to Exa on endpoints
    /// without a native search tool. Refuse instead (users can opt into another
    /// engine explicitly with `+s=exa`).
    async fn check_native_search(&self, r: &mut options::Resolved, model: &str) -> Result<(), LlmError> {
        let wants_native = r.body.get("tools").and_then(Value::as_array).is_some_and(|tools| {
            tools.iter().any(|t| {
                t["type"] == "openrouter:web_search"
                    && t.pointer("/parameters/engine").and_then(Value::as_str) == Some("native")
            })
        });
        if !wants_native {
            return Ok(());
        }
        let pinned: Vec<String> = r
            .body
            .pointer("/provider/only")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let eps = match self.endpoints(model).await {
            Ok(e) => e,
            Err(e) => {
                r.warnings.push(format!(
                    "could not verify native web search support ({e:#}); OpenRouter may fall back to Exa"
                ));
                return Ok(());
            }
        };
        let candidates: Vec<&EndpointInfo> = eps
            .iter()
            .filter(|e| {
                pinned.is_empty()
                    || e.base_slug().is_some_and(|b| pinned.iter().any(|p| p.split('/').next() == Some(b)))
            })
            .collect();
        if candidates.is_empty() {
            return Ok(()); // routing will fail with its own (verbatim) error
        }
        let native: Vec<&&EndpointInfo> =
            candidates.iter().filter(|e| e.has_native_tool("openrouter:web_search")).collect();
        if native.is_empty() {
            return Err(LlmError::request(format!(
                "{model} has no provider-native web search on the selected endpoint(s) [{}]; OpenRouter would silently use Exa instead. \
                 Use `+s=exa` (or another engine) to opt into OpenRouter's search explicitly.",
                candidates.iter().filter_map(|e| e.tag.clone()).collect::<Vec<_>>().join(", ")
            )));
        }
        if native.len() < candidates.len() {
            r.warnings
                .push("some of the allowed endpoints have no native web search; OpenRouter may use Exa there".into());
        }
        Ok(())
    }

    /// Run a prepared Chat Completions request, streaming events into `sink`.
    /// On success returns the native assistant payload for replay.
    pub async fn stream_chat(
        &self,
        p: &Prepared,
        sink: &mut dyn EventSink,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> (Result<Value, LlmError>, CallOutcome) {
        let mut outcome = CallOutcome::default();
        observer.request(&p.url, &p.body);
        let mut rb = self.auth(self.http.post(&p.url)).json(&p.body);
        if self.cfg.router_metadata {
            rb = rb.header("X-OpenRouter-Metadata", "enabled");
        }
        let send = rb.send();
        let resp = tokio::select! {
            _ = cancel.cancelled() => return (Err(LlmError::cancelled()), outcome),
            r = tokio::time::timeout(self.cfg.idle_timeout, send) => match r {
                Err(_) => return (Err(LlmError::new(ErrorKind::Timeout, Retry::Backoff, "no response headers within idle timeout")), outcome),
                Ok(Err(e)) => return (Err(LlmError::network(format!("request failed: {e}"))), outcome),
                Ok(Ok(r)) => r,
            }
        };
        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).to_string()))
            .collect();
        observer.response_status(status, &headers);
        let gen_id = resp.headers().get("x-generation-id").and_then(|v| v.to_str().ok()).map(str::to_string);
        if let Some(g) = &gen_id {
            outcome.upstream_id = Some(g.clone());
            sink.event(StreamEvent::Meta { upstream_id: Some(g.clone()), provider: None, model: None });
        }
        let retry_after =
            resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<u64>().ok());
        if !(200..300).contains(&status) {
            let text = resp.text().await.unwrap_or_default();
            observer.body_text(&text);
            let err = match serde_json::from_str::<Value>(&text) {
                Ok(v) => {
                    let e = v.get("error").cloned().unwrap_or(v.clone());
                    let mut le = chat::error_from_json(&e, Some(status), retry_after, ErrorKind::Http);
                    le.details = Some(v);
                    le
                }
                Err(_) => {
                    let mut le = LlmError::new(
                        ErrorKind::Http,
                        crate::error::retry_for_status(status, retry_after),
                        format!("HTTP {status}"),
                    );
                    le.status = Some(status);
                    le.details = Some(Value::String(text));
                    le
                }
            };
            return (Err(err), outcome);
        }

        let mut parser = SseParser::new();
        let mut cp = chat::ChunkParser::default();
        let mut body = resp.bytes_stream();
        let mut done = false;
        let result: Result<(), LlmError> = loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => break Err(LlmError::cancelled()),
                n = tokio::time::timeout(self.cfg.idle_timeout, body.next()) => n,
            };
            let chunk = match next {
                Err(_) => {
                    let mut e = LlmError::new(
                        ErrorKind::Timeout,
                        Retry::Backoff,
                        format!("stream idle for {}s", self.cfg.idle_timeout.as_secs()),
                    );
                    e.partial_output = cp.any_output;
                    break Err(e);
                }
                Ok(None) => break Ok(()),
                Ok(Some(Err(e))) => {
                    let mut le = LlmError::network(format!("stream read error: {e}"));
                    le.partial_output = cp.any_output;
                    break Err(le);
                }
                Ok(Some(Ok(b))) => b,
            };
            let mut items = parser.feed(&chunk);
            if done {
                items.clear();
            }
            match self.handle_items(items, &mut cp, sink, observer, &mut done) {
                Ok(()) => {}
                Err(e) => break Err(e),
            }
            if done {
                break Ok(());
            }
        };
        if result.is_ok() && !done {
            let tail = parser.finish();
            if let Err(e) = self.handle_items(tail, &mut cp, sink, observer, &mut done) {
                outcome.upstream_echo = std::mem::take(&mut cp.upstream_body_echo);
                return (Err(e), outcome);
            }
        }
        outcome.upstream_echo = std::mem::take(&mut cp.upstream_body_echo);
        outcome.router_metadata = cp.router_metadata.take();
        match result {
            Err(e) => (Err(e), outcome),
            Ok(()) => {
                if !cp.finished && !done {
                    let mut e = LlmError::network("stream ended without a finish reason or [DONE]");
                    e.partial_output = cp.any_output;
                    return (Err(e), outcome);
                }
                (Ok(cp.native_payload()), outcome)
            }
        }
    }

    fn handle_items(
        &self,
        items: Vec<SseItem>,
        cp: &mut chat::ChunkParser,
        sink: &mut dyn EventSink,
        observer: &dyn Observer,
        done: &mut bool,
    ) -> Result<(), LlmError> {
        for item in items {
            match item {
                SseItem::Comment(c) => observer.sse_comment(&c),
                SseItem::Event(ev) => {
                    observer.sse_data(&ev.data);
                    let data = ev.data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    if data == "[DONE]" {
                        *done = true;
                        return Ok(());
                    }
                    let v: Value = serde_json::from_str(data).map_err(|e| {
                        LlmError::new(ErrorKind::Decode, Retry::Backoff, format!("bad JSON chunk: {e}"))
                            .with_details(Value::String(data.to_string()))
                    })?;
                    for e in cp.chunk(&v)? {
                        sink.event(e);
                    }
                }
            }
        }
        Ok(())
    }
}
