//! OpenRouter client.

pub mod catalog;
pub mod chat;
pub mod messages;
pub mod options;
pub mod responses;
#[cfg(test)]
mod recorded;

use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::error::LlmError;
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

    /// Build a request in the skin closest to the vendor's own API (see
    /// [`options::dialect_for`]), after validating the options against what
    /// the vendor offers and what OpenRouter can deliver.
    ///
    /// `presets` are default options from the config (applied before the user's).
    pub async fn prepare_chat(
        &self,
        input: &CallInput<'_>,
        presets: &[crate::request::Requested],
        blobs: &dyn BlobLoader,
    ) -> Result<Prepared, LlmError> {
        let model = input.model;
        let info = self.model_info(model).await;
        let dialect = options::dialect_for(model);
        let user = crate::request::parse(input.options).map_err(LlmError::request)?;
        let mut req = crate::request::Requested::default();
        for p in presets {
            req = options::overlay(&req, p);
        }
        let req = options::overlay(&req, &user);
        // Pin: the user's choice, else the vendor's own endpoint.
        let pinned: Vec<String> = match &req.providers {
            Some(p) => p.iter().map(|s| s.split('/').next().unwrap_or(s).to_string()).collect(),
            None if self.cfg.fidelity.pin_first_party => {
                self.cfg.fidelity.first_party_for(model).map(|s| vec![s.to_string()]).unwrap_or_default()
            }
            None => Vec::new(),
        };
        // Endpoint metadata decides tools and tiers: fetch it if a request needs it.
        let needs_endpoints = !req.tools.is_empty() || req.tier.is_some();
        let endpoints: std::sync::Arc<Vec<EndpointInfo>> = match self.catalog.endpoints(model) {
            Some(e) => e,
            None if needs_endpoints => self
                .endpoints(model)
                .await
                .map_err(|e| LlmError::request(format!("cannot check what OpenRouter offers for {model}: {e:#}")))?,
            None => std::sync::Arc::new(Vec::new()),
        };
        let caps = options::OrCaps { model, info: info.as_deref(), endpoints: &endpoints, pinned, dialect };
        let auto_max = match input.max_tokens_override {
            Some(v) => Some(v),
            None => self.auto_max_tokens(input, info.as_deref()),
        };
        let plan = options::plan(model, info.as_deref(), &caps, &req, &user, auto_max).map_err(LlmError::request)?;
        let mut warnings = Vec::new();
        if info.is_none() {
            warnings.push(format!(
                "model `{model}` is not in OpenRouter's catalog (yet); sending without capability-based defaults"
            ));
        }
        let (mut body, notes, path, mut applied) = match dialect {
            options::Dialect::Chat => {
                let (messages, notes) = chat::convert_history(input.history, model, info.as_deref(), blobs).await?;
                let mut b = chat::body(model, messages, &plan);
                let mut applied = Vec::new();
                if notes.needs_file_parser {
                    // Force native file handling; router-side parsing is detected via annotations.
                    b["plugins_file_parser"] = json!(true);
                    applied.push(options::Applied {
                        source: "fidelity".into(),
                        patch: json!({"plugins": [{"id": "file-parser", "pdf": {"engine": "native"}}]}),
                    });
                }
                (b, notes.notes, "/chat/completions", applied)
            }
            options::Dialect::Responses => {
                let detail = plan.image_detail.unwrap_or("high");
                let (items, notes) =
                    responses::convert_history(input.history, model, info.as_deref(), detail, blobs).await?;
                (responses::body(model, items, &plan), notes, "/responses", Vec::new())
            }
            options::Dialect::Messages => {
                let (system, messages, notes) =
                    messages::convert_history(input.history, model, info.as_deref(), blobs).await?;
                let budget_models = info
                    .as_deref()
                    .and_then(|i| i.reasoning.as_ref())
                    .is_some_and(|r| r.supported_efforts.is_none() && r.supports_max_tokens == Some(true));
                (messages::body(model, system, messages, &plan, budget_models), notes, "/messages", Vec::new())
            }
        };
        // OpenRouter fields: routing pin, no context compression, upstream echo.
        let routing = options::routing(&plan, &self.cfg.fidelity);
        omni_core::json::merge_patch(&mut body, &routing);
        if body.get("plugins_file_parser").is_some() {
            if let Some(o) = body.as_object_mut() {
                o.remove("plugins_file_parser");
            }
            let mut plugins = body["plugins"].as_array().cloned().unwrap_or_default();
            plugins.push(json!({"id": "file-parser", "pdf": {"engine": "native"}}));
            body["plugins"] = Value::Array(plugins);
        }
        if self.cfg.echo_upstream_body && dialect != options::Dialect::Messages {
            body["debug"] = json!({"echo_upstream_body": true});
        }
        if let Some(s) = &input.session_id {
            body["session_id"] = json!(s);
        }
        if let Some(u) = &input.user {
            body["user"] = json!(u);
        }
        let mut all_applied = plan.applied.clone();
        all_applied.append(&mut applied);
        all_applied.push(options::Applied { source: "fidelity".into(), patch: routing });
        Ok(Prepared {
            dialect: match dialect {
                options::Dialect::Chat => chat::DIALECT,
                options::Dialect::Responses => responses::DIALECT,
                options::Dialect::Messages => messages::DIALECT,
            },
            model: model.to_string(),
            url: self.url(path),
            body,
            applied: all_applied,
            warnings,
            notes,
            show_debug: false,
            auto_max_tokens: auto_max,
            long_decode: dialect == options::Dialect::Chat && self.cfg.fidelity.is_long_decode(model),
            settings: plan.described.clone(),
        })
    }

    /// The endpoint the request will most likely be served by (pinned provider), from cache.
    /// Cache misses trigger a background refresh, so request latency never grows.
    fn cached_endpoint(&self, model: &str, options: &[omni_core::trigger::OptionToken]) -> Option<EndpointInfo> {
        let user_slug = options.iter().rev().find_map(|o| match o {
            omni_core::trigger::OptionToken::KeyValue(k, v)
                if (k == "p" || k == "provider") && v != "any" && v != "auto" =>
            {
                v.split('|').next().map(str::to_string)
            }
            _ => None,
        });
        let slug = user_slug.or_else(|| self.cfg.fidelity.first_party_for(model).map(str::to_string))?;
        match self.catalog.endpoints(model) {
            Some(eps) => {
                let base = slug.split('/').next().unwrap_or(&slug).to_string();
                eps.iter()
                    .find(|e| e.tag.as_deref() == Some(slug.as_str()))
                    .or_else(|| eps.iter().find(|e| e.base_slug() == Some(base.as_str())))
                    .cloned()
            }
            None => {
                let this = self.clone();
                let m = model.to_string();
                tokio::spawn(async move {
                    let _ = this.endpoints(&m).await;
                });
                None
            }
        }
    }

    /// Automatic `max_tokens`: as large as the model allows, within the context window.
    fn auto_max_tokens(&self, input: &CallInput<'_>, info: Option<&ModelInfo>) -> Option<u64> {
        let ep = self.cached_endpoint(input.model, input.options);
        let ctx = ep.as_ref().and_then(|e| e.context_length).or(info.and_then(|i| i.context_length));
        let prompt = crate::budget::estimate_prompt(input.history, input.known_context);
        if self.cfg.fidelity.is_long_decode(input.model) {
            // Long decode: the budget spans several continuation hops; the context window is unified in+out.
            return crate::budget::choose(ctx, Some(self.cfg.fidelity.long_decode_budget), None, prompt);
        }
        let model_max = ep
            .as_ref()
            .and_then(|e| e.max_completion_tokens)
            .or(info.and_then(|i| i.top_provider.max_completion_tokens));
        crate::budget::choose(ctx, model_max, self.cfg.fidelity.max_tokens_cap, prompt)
    }

    /// Prefetch endpoint metadata (startup), so budgeting never waits on the network.
    pub async fn warm_endpoints(&self, models: &[String]) {
        for m in models {
            if let Err(e) = self.endpoints(m).await {
                debug!(model = %m, error = %format!("{e:#}"), "endpoint prefetch failed");
            }
        }
    }

    /// `GET /generation/content?id=` (requires Input & Output Logging on the account):
    /// the stored prompt and the output *as delivered before any disconnect*.
    pub async fn generation_content(&self, id: &str) -> anyhow::Result<Option<Value>> {
        let resp = self
            .auth(self.http.get(self.url("/generation/content")).query(&[("id", id)]))
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        let v: Value = resp.error_for_status()?.json().await?;
        Ok(v.get("data").cloned())
    }

    /// Run a prepared request (any skin), streaming events into `sink`.
    /// On success returns the native assistant payload for replay.
    pub async fn stream_chat(
        &self,
        p: &Prepared,
        sink: &mut dyn EventSink,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> (Result<Value, LlmError>, CallOutcome) {
        let mut rb = self.auth(self.http.post(&p.url)).json(&p.body);
        if self.cfg.router_metadata {
            rb = rb.header("X-OpenRouter-Metadata", "enabled");
        }
        let idle = self.cfg.idle_timeout;
        let hdr = Some("x-generation-id");
        match p.dialect {
            responses::DIALECT => {
                let crate::chatstream::Streamed { result, parser: mut ep, header_id } = crate::chatstream::run_with(
                    rb,
                    &p.url,
                    &p.body,
                    idle,
                    hdr,
                    chat::error_from_json,
                    responses::EventParser::default(),
                    sink,
                    observer,
                    cancel,
                )
                .await;
                let outcome = CallOutcome {
                    upstream_id: header_id,
                    upstream_echo: std::mem::take(&mut ep.upstream_body_echo),
                    ..Default::default()
                };
                (result.map(|()| ep.native_payload()), outcome)
            }
            messages::DIALECT => {
                let crate::chatstream::Streamed { result, parser: ep, header_id } = crate::chatstream::run_with(
                    rb,
                    &p.url,
                    &p.body,
                    idle,
                    hdr,
                    chat::error_from_json,
                    messages::EventParser::default(),
                    sink,
                    observer,
                    cancel,
                )
                .await;
                (result.map(|()| ep.native_payload()), CallOutcome { upstream_id: header_id, ..Default::default() })
            }
            _ => {
                let crate::chatstream::Streamed { result, parser: mut cp, header_id } = crate::chatstream::run(
                    rb,
                    &p.url,
                    &p.body,
                    idle,
                    hdr,
                    chat::error_from_json,
                    sink,
                    observer,
                    cancel,
                )
                .await;
                let outcome = CallOutcome {
                    upstream_id: header_id,
                    continuation: cp.continuation().cloned(),
                    upstream_echo: std::mem::take(&mut cp.upstream_body_echo),
                    router_metadata: cp.router_metadata.take(),
                };
                (result.map(|()| cp.native_payload()), outcome)
            }
        }
    }
}
