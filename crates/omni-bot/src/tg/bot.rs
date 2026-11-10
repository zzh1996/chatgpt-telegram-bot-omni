//! One Telegram bot connection (MTProto via grammers).

use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use grammers_client::Client;
use grammers_client::client::UpdateStream;
use grammers_mtsender::{
    ConnectionParams, InvocationError, RetryContext, RetryPolicy, SenderPool, SenderPoolFatHandle, UpdatesConfiguration,
};
use grammers_session::Session as _;
use grammers_session::types::PeerId;
use grammers_tl_types as tl;
use rusqlite::params;
use tracing::{info, warn};

use super::gate::Gates;
use crate::config::BotConfig;
use crate::store::Db;
use crate::store::session::BotSession;

/// Library-level retry: short flood waits and transient I/O errors.
/// Longer flood waits are surfaced to callers (the outbox handles them).
struct Policy;

impl RetryPolicy for Policy {
    fn should_retry(&self, ctx: &RetryContext) -> ControlFlow<(), Duration> {
        match &ctx.error {
            InvocationError::Rpc(e) if e.code == 420 => match e.value {
                Some(s) if s <= 10 && ctx.fail_count.get() <= 3 => {
                    ControlFlow::Continue(Duration::from_secs(s as u64 + 1))
                }
                _ => ControlFlow::Break(()),
            },
            InvocationError::Io(_) if ctx.fail_count.get() <= 3 => ControlFlow::Continue(Duration::from_secs(1)),
            _ => ControlFlow::Break(()),
        }
    }
}

pub struct BotConn {
    pub name: String,
    pub user_id: i64,
    pub username: String,
    pub client: Client,
    pub session: Arc<BotSession>,
    pub gates: Gates,
    handle: SenderPoolFatHandle,
}

impl BotConn {
    /// Connect, sign in (if needed) and start receiving updates (with catch-up).
    pub async fn connect(
        db: &Db,
        cfg: &BotConfig,
        api_id: i32,
        api_hash: &str,
        gates: Gates,
    ) -> Result<(Arc<BotConn>, UpdateStream)> {
        let token_user_id = crate::config::Config::bot_id_of(&cfg.token).context("bad token")?;
        reset_session_if_token_changed(db, &cfg.name, token_user_id).await?;

        let session = Arc::new(BotSession::load(db.clone(), &cfg.name)?);
        let params = ConnectionParams {
            retry_policy: Box::new(Policy),
            updates_channel_capacity: NonZeroUsize::new(20_000).unwrap(),
            device_model: "omni".into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        };
        let SenderPool { runner, updates, handle } = SenderPool::with_configuration(session.clone(), api_id, params);
        let client = Client::new(handle.clone());
        let name = cfg.name.clone();
        tokio::spawn(async move {
            runner.run().await;
            info!(bot = %name, "sender pool stopped");
        });

        if !client.is_authorized().await.context("check authorization")? {
            info!(bot = %cfg.name, "signing in with bot token");
            client.bot_sign_in(&cfg.token, api_hash).await.context("bot sign in")?;
        }
        let me = client.get_me().await.context("get_me")?;
        let user_id = me.id().bare_id().unwrap_or(token_user_id);
        if user_id != token_user_id {
            bail!("bot {:?}: session belongs to user {user_id}, token to {token_user_id}", cfg.name);
        }
        let username = me.username().unwrap_or_default().to_string();
        db.meta_set(&format!("bot:{}:user_id", cfg.name), &user_id.to_string()).await?;
        info!(bot = %cfg.name, user_id, %username, "connected");

        let stream = client
            .stream_updates(updates, UpdatesConfiguration { catch_up: true })
            .await
            .map_err(|e| anyhow::anyhow!("start update stream: {e}"))?;
        let conn = Arc::new(BotConn { name: cfg.name.clone(), user_id, username, client, session, gates, handle });
        Ok((conn, stream))
    }

    pub fn quit(&self) {
        self.handle.quit();
    }

    pub async fn invoke<R: tl::RemoteCall>(&self, req: &R) -> Result<R::Return, InvocationError> {
        self.client.invoke(req).await
    }

    /// Resolve a marked chat id to an input peer, using cached access hashes.
    pub async fn input_peer(&self, chat_id: i64) -> Result<tl::enums::InputPeer> {
        let id = PeerId::from_bot_api_dialog_id(chat_id).with_context(|| format!("invalid chat id {chat_id}"))?;
        let r =
            self.session.peer_ref(id).await.map_err(|e| anyhow::anyhow!("{e}"))?.unwrap_or_else(|| id.to_ambient_ref());
        Ok((&r).into())
    }

    pub async fn input_channel(&self, chat_id: i64) -> Result<tl::enums::InputChannel> {
        let id = PeerId::from_bot_api_dialog_id(chat_id).with_context(|| format!("invalid chat id {chat_id}"))?;
        let r =
            self.session.peer_ref(id).await.map_err(|e| anyhow::anyhow!("{e}"))?.unwrap_or_else(|| id.to_ambient_ref());
        Ok((&r).into())
    }

    pub async fn input_user(&self, user_id: i64) -> Result<tl::enums::InputUser> {
        let id = PeerId::user(user_id).with_context(|| format!("invalid user id {user_id}"))?;
        let r =
            self.session.peer_ref(id).await.map_err(|e| anyhow::anyhow!("{e}"))?.unwrap_or_else(|| id.to_ambient_ref());
        Ok((&r).into())
    }

    /// Fetch messages by id (works for private chats, groups and supergroups).
    pub async fn get_messages(&self, chat_id: i64, ids: &[i32]) -> Result<Vec<tl::enums::Message>> {
        let id: Vec<tl::enums::InputMessage> =
            ids.iter().map(|&i| tl::types::InputMessageId { id: i }.into()).collect();
        let res = if super::convert::chat_kind(chat_id) == super::convert::ChatKind::Channel {
            let channel = self.input_channel(chat_id).await?;
            self.invoke(&tl::functions::channels::GetMessages { channel, id }).await?
        } else {
            self.invoke(&tl::functions::messages::GetMessages { id }).await?
        };
        Ok(match res {
            tl::enums::messages::Messages::Messages(m) => m.messages,
            tl::enums::messages::Messages::Slice(m) => m.messages,
            tl::enums::messages::Messages::ChannelMessages(m) => m.messages,
            tl::enums::messages::Messages::NotModified(_) => Vec::new(),
        })
    }
}

async fn reset_session_if_token_changed(db: &Db, bot: &str, token_user_id: i64) -> Result<()> {
    let key = format!("bot:{bot}:user_id");
    let Some(prev) = db.meta_get(&key).await? else { return Ok(()) };
    if prev.parse::<i64>().ok() == Some(token_user_id) {
        return Ok(());
    }
    warn!(bot, prev = %prev, new = token_user_id, "bot token now belongs to a different bot; resetting its Telegram session");
    let bot = bot.to_string();
    db.call(move |c| {
        let tx = c.transaction()?;
        for t in ["tg_session", "tg_dc", "tg_peer", "tg_update_state", "tg_channel_state"] {
            tx.execute(&format!("DELETE FROM {t} WHERE bot = ?1"), params![bot])?;
        }
        tx.execute("DELETE FROM meta WHERE key = ?1", params![format!("bot:{bot}:user_id")])?;
        tx.commit()?;
        Ok(())
    })
    .await
}
