//! Shared application state and model resolution. Every bot is independent:
//! it answers the prefixes it serves, with its own aliases and access rules.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use omni_core::trigger::{OptionToken, SpecToken, parse_spec};
use omni_llm::openrouter::OpenRouter;
use parking_lot::{Mutex, RwLock};
use rusqlite::params;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::config::{BotConfig, Config, glob_match};

use crate::conv::ResolvedSpec;
use crate::store::blobs::BlobStore;
use crate::store::{Db, now_ms};
use crate::tg::bot::BotConn;

pub struct App {
    pub cfg: Config,
    pub db: Db,
    pub blobs: BlobStore,
    pub openrouter: OpenRouter,
    /// The platforms' own APIs that are configured (have a key), by id.
    pub direct: BTreeMap<String, Arc<dyn omni_llm::provider::Provider>>,
    /// Official providers per model author that users may pick (`+p=`).
    pub allowed_providers: BTreeMap<String, Vec<String>>,
    /// Bots in config order.
    pub bots: Vec<Arc<BotConn>>,
    /// (bot name, chat id) pairs where the bot is known to be a member.
    pub membership: Mutex<HashSet<(String, i64)>>,
    /// Per bot: the config's aliases overlaid with the bot's runtime ones
    /// (`/alias`): bot -> name (may be empty) -> spec string.
    pub aliases: RwLock<BTreeMap<String, BTreeMap<String, String>>>,
    /// Chat prefixes: (bot, chat) -> name -> prefix.
    pub chat_prefixes: RwLock<HashMap<(String, i64), BTreeMap<String, Arc<crate::prefixes::ChatPrefix>>>>,
    /// Assistant turns currently being generated (dedupe of job runners).
    pub running: Mutex<HashSet<i64>>,
    /// Inbound messages currently being handled (dedupe of dispatches: the
    /// startup recovery and the update loop may dispatch the same one).
    pub handling: Mutex<HashSet<i64>>,
    /// Fires with a turn id whenever an assistant turn finishes.
    pub turn_done: broadcast::Sender<i64>,
    pub shutdown: CancellationToken,
    pub started_at: jiff::Timestamp,
}

/// Every provider id the bot knows (`openrouter` and the platforms' own APIs).
pub fn is_provider(id: &str) -> bool {
    id == "openrouter" || omni_llm::provider::info(id).is_some()
}

impl App {
    pub fn bot(&self, name: &str) -> Option<&Arc<BotConn>> {
        self.bots.iter().find(|b| b.name == name)
    }

    /// What ends the model prefix of a message (`$` unless configured otherwise).
    pub fn marker(&self) -> &str {
        &self.cfg.general.trigger_marker
    }

    pub fn bot_cfg(&self, name: &str) -> Option<&BotConfig> {
        self.cfg.bots.iter().find(|b| b.name == name)
    }

    pub fn is_admin(&self, user_id: i64) -> bool {
        self.cfg.access.admins.contains(&user_id)
    }

    pub fn our_bot_ids(&self) -> HashSet<i64> {
        self.bots.iter().map(|b| b.user_id).collect()
    }

    pub async fn load_runtime_state(&self) -> Result<()> {
        let config: BTreeMap<String, String> =
            self.cfg.aliases.iter().map(|a| (a.name.clone(), a.spec.clone())).collect();
        let mut aliases: BTreeMap<String, BTreeMap<String, String>> =
            self.cfg.bots.iter().map(|b| (b.name.clone(), config.clone())).collect();
        let (db_aliases, members) = self
            .db
            .call(|c| {
                let mut s = c.prepare("SELECT bot, name, spec FROM aliases")?;
                let a = s
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                let mut s = c.prepare("SELECT bot, chat_id FROM bot_chats WHERE status = 'member'")?;
                let m = s
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((a, m))
            })
            .await?;
        for (bot, name, spec) in db_aliases {
            if let Some(m) = aliases.get_mut(&bot) {
                m.insert(name, spec);
            }
        }
        *self.aliases.write() = aliases;
        let mut prefixes: HashMap<(String, i64), BTreeMap<String, Arc<crate::prefixes::ChatPrefix>>> = HashMap::new();
        for (bot, chat, name, p) in crate::prefixes::load_all(&self.db).await? {
            prefixes.entry((bot, chat)).or_default().insert(name, Arc::new(p));
        }
        *self.chat_prefixes.write() = prefixes;
        self.membership.lock().extend(members);
        Ok(())
    }

    /// Record that `bot` is (or is no longer) a member of `chat`.
    pub async fn set_member(&self, bot: &str, chat: i64, member: bool) -> Result<()> {
        let key = (bot.to_string(), chat);
        let changed = {
            let mut m = self.membership.lock();
            if member { m.insert(key) } else { m.remove(&key) }
        };
        if changed {
            let bot = bot.to_string();
            self.db
                .call(move |c| {
                    c.execute(
                        "INSERT INTO bot_chats(bot, chat_id, status, updated_at) VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(bot, chat_id) DO UPDATE SET status = excluded.status, updated_at = excluded.updated_at",
                        params![bot, chat, if member { "member" } else { "left" }, now_ms()],
                    )?;
                    Ok(())
                })
                .await?;
        }
        Ok(())
    }

    /// A chat's own prefix (names are lowercase letters and digits).
    pub fn chat_prefix(&self, bot: &str, chat_id: i64, name: &str) -> Option<Arc<crate::prefixes::ChatPrefix>> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        self.chat_prefixes.read().get(&(bot.to_string(), chat_id))?.get(&name.to_ascii_lowercase()).cloned()
    }

    /// A chat's own prefixes, by name.
    pub fn chat_prefixes_of(&self, bot: &str, chat_id: i64) -> BTreeMap<String, Arc<crate::prefixes::ChatPrefix>> {
        self.chat_prefixes.read().get(&(bot.to_string(), chat_id)).cloned().unwrap_or_default()
    }

    /// Whether a name is already a prefix for this chat: the bot's (any case) or the chat's own.
    pub fn prefix_taken(&self, bot: &str, chat_id: i64, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        self.aliases_of(bot).keys().any(|k| k.to_ascii_lowercase() == lower)
            || self.chat_prefix(bot, chat_id, name).is_some()
    }

    /// A bot's alias (the empty name is the `$text` prefix).
    pub fn alias(&self, bot: &str, name: &str) -> Option<String> {
        self.aliases.read().get(bot)?.get(name).cloned()
    }

    /// All aliases of a bot, by name (the empty name first).
    pub fn aliases_of(&self, bot: &str) -> BTreeMap<String, String> {
        self.aliases.read().get(bot).cloned().unwrap_or_default()
    }

    /// A bot's prefixes for display: the config's in file order, then the
    /// runtime-only ones by name.
    pub fn aliases_in_order(&self, bot: &str) -> Vec<(String, String)> {
        let all = self.aliases_of(bot);
        let mut out: Vec<(String, String)> =
            self.cfg.aliases.iter().filter_map(|a| Some((a.name.clone(), all.get(&a.name)?.clone()))).collect();
        for (n, s) in &all {
            if !self.cfg.aliases.iter().any(|a| &a.name == n) {
                out.push((n.clone(), s.clone()));
            }
        }
        out
    }

    /// Set (`Some`) or remove a runtime alias of a bot in memory; removing one
    /// brings back the config's alias of that name, if any.
    pub fn set_runtime_alias(&self, bot: &str, name: &str, spec: Option<String>) {
        let mut all = self.aliases.write();
        let Some(m) = all.get_mut(bot) else { return };
        match spec.or_else(|| self.cfg.aliases.iter().find(|a| a.name == name).map(|a| a.spec.clone())) {
            Some(s) => m.insert(name.to_string(), s),
            None => m.remove(name),
        };
    }

    /// Whether a provider exists in this deployment (a platform's own API only
    /// when it has a key).
    /// A provider exists only with an API key (D43): OpenRouter too.
    pub fn provider_enabled(&self, provider: &str) -> bool {
        match provider {
            "openrouter" => self.cfg.providers.openrouter.as_ref().is_some_and(|c| !c.api_key.trim().is_empty()),
            id => self.direct.contains_key(id),
        }
    }

    /// Total output budget of a long-decode answer (Gemini 4) on `provider`.
    pub fn long_decode_budget(&self, provider: &str) -> u64 {
        match provider {
            "openrouter" => self.openrouter.config().fidelity.long_decode_budget,
            id => self.cfg.providers.direct(id).map(|c| c.long_decode_budget).unwrap_or(393_216),
        }
    }

    /// A configured platform API.
    pub fn direct(&self, provider: &str) -> Option<&Arc<dyn omni_llm::provider::Provider>> {
        self.direct.get(provider)
    }

    /// Whether users may type model ids of `provider` that no prefix names
    /// (`allow_unlisted_models` of the provider's config).
    pub fn unlisted_models_allowed(&self, provider: &str) -> bool {
        let p = &self.cfg.providers;
        self.provider_enabled(provider)
            && match provider {
                "openrouter" => p.openrouter.as_ref().is_some_and(|c| c.allow_unlisted_models),
                id => p.direct(id).is_some_and(|c| c.allow_unlisted_models),
            }
    }

    /// The canonical model id (`provider:model`) of a spec string, if it names
    /// a model of an enabled provider that `bot` serves.
    pub fn served_canonical(&self, bot: &BotConfig, spec: &str) -> Option<String> {
        let model = parse_spec(spec).ok()?.model;
        let (provider, model) = match model.split_once(':') {
            Some((p, m)) if is_provider(p) => (p, m),
            _ => ("openrouter", model.as_str()),
        };
        let canonical = format!("{provider}:{model}");
        let ok = self.provider_enabled(provider)
            && !model.is_empty()
            && (provider != "openrouter" || model.contains('/'))
            && bot.serves(&canonical)
            // A platform's model the bot has no price for cannot be used.
            && self.direct(provider).is_none_or(|p| p.knows(model));
        ok.then_some(canonical)
    }

    /// Resolve a typed spec into provider + model + options, for one bot in one
    /// chat: the bot's prefixes first, then the chat's own ones (which carry a
    /// system prompt and history), then model ids.
    /// `None`: not a model this bot knows (no such alias or model, a provider
    /// that is not configured, a model the bot does not have, or a model id
    /// typed directly where the provider does not allow unlisted models).
    ///
    /// `strict`: only accept raw model ids that exist in the catalog (used in
    /// groups, so that random text containing `a/b$` does not trigger requests).
    pub async fn resolve_spec(
        &self,
        token: &SpecToken,
        bot: &BotConfig,
        chat_id: i64,
        strict: bool,
    ) -> Result<Option<ResolvedSpec>, String> {
        let mut model_token = token.model.clone();
        let mut options: Vec<OptionToken> = Vec::new();
        let mut label = token.model.clone();
        let mut template = None;
        // Prefix expansion (one level). The empty name is an alias like any other.
        let alias = self.alias(&bot.name, &model_token);
        let chat_prefix = if alias.is_none() { self.chat_prefix(&bot.name, chat_id, &model_token) } else { None };
        let via_alias = alias.is_some() || chat_prefix.is_some();
        if let Some(spec) = alias.or_else(|| chat_prefix.as_ref().map(|p| p.spec.clone())) {
            let parsed = parse_spec(&spec)
                .map_err(|e| format!("prefix `{model_token}{}` has an invalid spec `{spec}`: {e}", self.marker()))?;
            model_token = parsed.model;
            options.extend(parsed.options);
            template = chat_prefix.map(|p| p.turns.clone());
        } else if model_token.is_empty() {
            return Ok(None);
        }
        options.extend(token.options.iter().cloned());
        let (provider, model) = match model_token.split_once(':') {
            Some((p, m)) if is_provider(p) => (p.to_string(), m.to_string()),
            _ => ("openrouter".to_string(), model_token.clone()),
        };
        // OpenRouter ids are `author/model`; other providers need their explicit prefix (`ark:…`).
        if provider == "openrouter" && !model.contains('/') {
            return Ok(None);
        }
        if model.is_empty() || !bot.serves(&format!("{provider}:{model}")) {
            return Ok(None);
        }
        if !self.provider_enabled(&provider) {
            // This bot would serve it, but the platform has no API key here: a prefix
            // of this bot or a platform named explicitly gets an error, never silence.
            // (A bare `author/model` in a group may be ordinary text: ignored there.)
            if via_alias || provider != "openrouter" || !strict {
                let name = omni_llm::provider::info(&provider).map(|i| i.name).unwrap_or("OpenRouter");
                return Err(format!(
                    "`{}{}` is not available: {name} is not configured on this bot (no API key)",
                    token.raw,
                    self.marker()
                ));
            }
            return Ok(None);
        }
        if !via_alias && !self.unlisted_models_allowed(&provider) {
            return Ok(None);
        }
        // Strict mode never blocks on the network (it runs in the update loop):
        // use the cached catalog and refresh it in the background on a miss.
        if strict && provider == "openrouter" && self.openrouter.catalog.get(&model).is_none() {
            self.refresh_catalog_soon();
            return Ok(None);
        }
        // A platform's model must be in its model table (anywhere, also through a
        // prefix): the bot cannot know the price of anything else (owner's rule).
        // A prefix or a private chat says so; in a group a typed id may be text.
        if let Some(p) = self.direct(&provider).filter(|p| !p.knows(&model)) {
            if via_alias || !strict {
                return Err(omni_llm::provider::not_in_table(p.name(), &model).message);
            }
            return Ok(None);
        }
        if label.contains(':') && label == model_token {
            label = model.clone();
        }
        Ok(Some(ResolvedSpec { provider, model, label, options, raw: token.raw.clone(), template }))
    }

    /// Whether `token` looks like a full OpenRouter model id (`author/model`)
    /// that `bot` would serve, possibly one too new for the cached catalog.
    pub fn plausible_model_id(&self, bot: &BotConfig, token: &SpecToken) -> bool {
        let m = token.model.strip_prefix("openrouter:").unwrap_or(&token.model);
        let Some((author, model)) = m.split_once('/') else { return false };
        let ok = |s: &str, extra: &str| {
            !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c) || extra.contains(c))
        };
        ok(author, "")
            && ok(model, ":")
            && self.alias(&bot.name, &token.model).is_none()
            && self.unlisted_models_allowed("openrouter")
            && bot.serves(&format!("openrouter:{m}"))
    }

    /// Refresh the catalog now unless it was refreshed in the last `max_age`
    /// (concurrent callers share one refresh). For ids missing from the cache.
    pub async fn refresh_catalog_if_older(&self, max_age: std::time::Duration) {
        static LOCK: tokio::sync::Mutex<i64> = tokio::sync::Mutex::const_new(0);
        let mut last = LOCK.lock().await;
        if now_ms() - *last < max_age.as_millis() as i64 {
            return;
        }
        match self.openrouter.refresh_catalog().await {
            Ok(n) => tracing::info!(models = n, "refreshed the model catalog for an unknown model id"),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "catalog refresh failed"),
        }
        *last = now_ms();
    }

    /// Refresh the model catalog in the background (at most once a minute).
    pub fn refresh_catalog_soon(&self) {
        use std::sync::atomic::{AtomicI64, Ordering};
        static LAST: AtomicI64 = AtomicI64::new(0);
        let now = now_ms();
        if now - LAST.load(Ordering::Relaxed) < 60_000 {
            return;
        }
        LAST.store(now, Ordering::Relaxed);
        let or = self.openrouter.clone();
        tokio::spawn(async move {
            if let Err(e) = or.refresh_catalog().await {
                tracing::warn!(error = %format!("{e:#}"), "background catalog refresh failed");
            }
        });
    }

    /// Default options from config presets matching a canonical model id.
    pub fn presets_for(&self, canonical: &str) -> Vec<omni_llm::request::Requested> {
        self.cfg
            .presets
            .iter()
            .filter(|p| glob_match(&p.pattern, canonical))
            .filter_map(|p| p.requested().ok())
            .collect()
    }

    /// Stable, salted hash of a Telegram user id for the upstream `user` field.
    pub fn user_hash(&self, user_id: i64) -> Option<String> {
        let salt = self.cfg.providers.openrouter.as_ref()?.user_id_salt.clone()?;
        if salt.is_empty() {
            return None;
        }
        use sha2::Digest;
        Some(hex::encode(sha2::Sha256::digest(format!("{salt}{user_id}").as_bytes())))
    }
}
