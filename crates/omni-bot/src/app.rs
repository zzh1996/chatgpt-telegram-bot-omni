//! Shared application state, model resolution and multi-bot routing.

use std::collections::{BTreeMap, HashSet};
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
    /// Volcengine Ark, when configured.
    pub ark: Option<omni_llm::ark::Ark>,
    /// Official providers per model author that users may pick (`+p=`).
    pub allowed_providers: BTreeMap<String, Vec<String>>,
    /// Bots in config order.
    pub bots: Vec<Arc<BotConn>>,
    /// (bot name, chat id) pairs where the bot is known to be a member.
    pub membership: Mutex<HashSet<(String, i64)>>,
    /// Aliases from config plus runtime (admin-defined) ones: name -> spec string.
    pub aliases: RwLock<BTreeMap<String, String>>,
    /// Assistant turns currently being generated (dedupe of job runners).
    pub running: Mutex<HashSet<i64>>,
    /// Fires with a turn id whenever an assistant turn finishes.
    pub turn_done: broadcast::Sender<i64>,
    pub shutdown: CancellationToken,
    pub started_at: jiff::Timestamp,
}

pub const PROVIDERS: &[&str] = &["openrouter", "ark"];

impl App {
    pub fn bot(&self, name: &str) -> Option<&Arc<BotConn>> {
        self.bots.iter().find(|b| b.name == name)
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
        let mut aliases: BTreeMap<String, String> =
            self.cfg.aliases.iter().map(|a| (a.name.clone(), a.spec.clone())).collect();
        let (db_aliases, members) = self
            .db
            .call(|c| {
                let mut s = c.prepare("SELECT name, spec FROM aliases")?;
                let a = s
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                let mut s = c.prepare("SELECT bot, chat_id FROM bot_chats WHERE status = 'member'")?;
                let m = s
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((a, m))
            })
            .await?;
        aliases.extend(db_aliases);
        *self.aliases.write() = aliases;
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

    /// Resolve a typed spec into provider + model + options.
    ///
    /// `strict`: only accept raw model ids that exist in the catalog (used in
    /// groups, so that random text containing `a/b$` does not trigger requests).
    pub async fn resolve_spec(
        &self,
        token: &SpecToken,
        bot: &BotConfig,
        strict: bool,
    ) -> Result<Option<ResolvedSpec>, String> {
        let mut model_token = token.model.clone();
        let mut options: Vec<OptionToken> = Vec::new();
        let mut label = token.model.clone();
        if model_token.is_empty() {
            match &bot.default_model {
                Some(d) => {
                    model_token = d.clone();
                    label = d.clone();
                }
                None => return Ok(None),
            }
        }
        // Alias expansion (one level).
        let alias = self.aliases.read().get(&model_token).cloned();
        if let Some(spec) = alias {
            let parsed =
                parse_spec(&spec).map_err(|e| format!("alias `{model_token}` has an invalid spec `{spec}`: {e}"))?;
            model_token = parsed.model;
            options.extend(parsed.options);
        }
        options.extend(token.options.iter().cloned());
        let (provider, model) = match model_token.split_once(':') {
            Some((p, m)) if PROVIDERS.contains(&p) => (p.to_string(), m.to_string()),
            _ => ("openrouter".to_string(), model_token.clone()),
        };
        // OpenRouter ids are `author/model`; other providers need their explicit prefix (`ark:…`).
        if provider == "openrouter" && !model.contains('/') {
            return Ok(None);
        }
        if model.is_empty() {
            return Ok(None);
        }
        // Strict mode never blocks on the network (it runs in the update loop):
        // use the cached catalog and refresh it in the background on a miss.
        if strict && provider == "openrouter" && self.openrouter.catalog.get(&model).is_none() {
            self.refresh_catalog_soon();
            return Ok(None);
        }
        if label.contains(':') && label == model_token {
            label = model.clone();
        }
        Ok(Some(ResolvedSpec { provider, model, label, options, raw: token.raw.clone() }))
    }

    /// Whether `token` looks like a full OpenRouter model id (`author/model`),
    /// possibly one that is too new for the cached catalog.
    pub fn plausible_model_id(&self, token: &SpecToken) -> bool {
        let m = token.model.strip_prefix("openrouter:").unwrap_or(&token.model);
        let Some((author, model)) = m.split_once('/') else { return false };
        let ok = |s: &str, extra: &str| {
            !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c) || extra.contains(c))
        };
        ok(author, "") && ok(model, ":") && !self.aliases.read().contains_key(&token.model)
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

    /// Which of our bots should answer requests for `spec` in `chat`.
    pub fn responsible_bot(&self, chat_id: i64, canonical: &str, receiving: &str) -> Option<String> {
        if chat_id > 0 {
            // Private chats are per bot: only the receiving bot can answer.
            let b = self.bot_cfg(receiving)?;
            return b.models.iter().any(|p| glob_match(p, canonical)).then(|| receiving.to_string());
        }
        let members = self.membership.lock();
        for b in &self.cfg.bots {
            if !b.models.iter().any(|p| glob_match(p, canonical)) {
                continue;
            }
            if let Some(chats) = &b.chats
                && !chats.contains(&chat_id)
            {
                continue;
            }
            if b.name == receiving || members.contains(&(b.name.clone(), chat_id)) {
                return Some(b.name.clone());
            }
        }
        None
    }

    /// The bot that answers un-addressed commands in a chat (first member in config order).
    pub fn primary_bot(&self, chat_id: i64, receiving: &str) -> String {
        if chat_id > 0 {
            return receiving.to_string();
        }
        let members = self.membership.lock();
        for b in &self.cfg.bots {
            if b.name == receiving || members.contains(&(b.name.clone(), chat_id)) {
                return b.name.clone();
            }
        }
        receiving.to_string()
    }

    /// Config presets (JSON merge patches) matching a canonical model id.
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
