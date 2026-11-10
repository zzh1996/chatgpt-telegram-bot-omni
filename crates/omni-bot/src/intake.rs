//! Update intake: one sequential loop per bot.
//!
//! For every update: decide quickly (no network) whether this bot must act on
//! it; if so, record it durably in `inbound` (unique per bot/chat/message) and
//! dispatch the handler.
//!
//! The persisted update state (pts/qts/channel pts) is advanced only *after*
//! every update carrying that state has been processed. This matters because
//! grammers tags all messages of one `getDifference` batch with the batch's
//! final state: we persist a state when an update with a different state
//! arrives (the previous group is then complete). A crash can therefore only
//! cause re-delivery of already-processed updates, which the unique key of
//! `inbound` absorbs — nothing is lost and nothing is processed twice.

use std::sync::Arc;

use anyhow::Result;
use grammers_client::client::UpdateStream;
use grammers_client::peer::{Peer, PeerMap};
use grammers_session::types::{PeerId, UpdateState};
use grammers_session::updates::{MessageBox, State};
use grammers_tl_types as tl;
use rusqlite::params;
use tracing::{Instrument, debug, info, info_span, warn};

use crate::app::App;
use crate::conv::InboundPayload;
use crate::media::media_of;
use crate::store::now_ms;
use crate::store::session::BotSession;
use crate::tg::bot::BotConn;
use crate::tg::convert::marked_peer_id;

pub const COMMANDS: &[&str] = &[
    "start",
    "help",
    "ping",
    "whoami",
    "models",
    "model",
    "raw",
    "retry",
    "m",
    "info",
    "stats",
    "usage",
    "allow",
    "deny",
    "allow_user",
    "deny_user",
    "ban",
    "unban",
    "acl",
    "quota",
    "alias",
    "unalias",
    "credits",
];

fn update_state(s: &State) -> Option<UpdateState> {
    match s.message_box? {
        MessageBox::Common { pts } => Some(UpdateState::Primary { pts, date: s.date, seq: s.seq }),
        MessageBox::Secondary { qts } => Some(UpdateState::Secondary { qts }),
        MessageBox::Channel { channel_id, pts } => Some(UpdateState::Channel { id: channel_id, pts }),
    }
}

/// What the bot should do with a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Command(String),
    Reply,
    Trigger,
    /// Private-chat message we cannot interpret (reply with a hint).
    Hint,
}

pub async fn run(app: Arc<App>, bot: Arc<BotConn>, mut stream: UpdateStream) {
    let span = info_span!("bot", bot = %bot.name);
    async move {
        info!("update loop started");
        let mut prev: Option<State> = None;
        loop {
            let next = tokio::select! {
                _ = app.shutdown.cancelled() => break,
                n = stream.next_raw() => n,
            };
            let (update, state, peers) = match next {
                Ok(x) => x,
                Err(e) => {
                    warn!(error = %e, "update stream error; retrying");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
            };
            if prev.as_ref() != Some(&state) {
                // All updates of the previous state group have been processed.
                if let Some(p) = prev.take()
                    && let Err(e) = persist_state(&app, &bot.name, update_state(&p)).await
                {
                    warn!(error = %format!("{e:#}"), "failed to persist update state");
                }
                prev = Some(state.clone());
            }
            if let Err(e) = on_update(&app, &bot, update, &peers).await {
                warn!(error = %format!("{e:#}"), "update handling failed");
            }
        }
        // Deliberately not syncing the in-memory message box state here: it may
        // already include buffered updates that were not processed. Re-delivery
        // after restart is harmless (deduplicated).
        drop(stream);
        info!("update loop stopped");
    }
    .instrument(span)
    .await
}

async fn persist_state(app: &App, bot: &str, st: Option<UpdateState>) -> Result<()> {
    let Some(st) = st else { return Ok(()) };
    let bot = bot.to_string();
    app.db
        .call(move |c| {
            BotSession::apply_update_state_in(c, &bot, &st)?;
            Ok(())
        })
        .await
}

async fn on_update(app: &Arc<App>, bot: &Arc<BotConn>, update: tl::enums::Update, peers: &PeerMap) -> Result<()> {
    let msg = match update {
        tl::enums::Update::NewMessage(u) => u.message,
        tl::enums::Update::NewChannelMessage(u) => u.message,
        tl::enums::Update::ChatParticipant(u) => {
            if u.user_id == bot.user_id {
                let member = u.new_participant.is_some();
                app.set_member(&bot.name, -u.chat_id, member).await?;
            }
            return Ok(());
        }
        tl::enums::Update::ChannelParticipant(u) => {
            if u.user_id == bot.user_id {
                let member = matches!(
                    u.new_participant,
                    Some(tl::enums::ChannelParticipant::Participant(_))
                        | Some(tl::enums::ChannelParticipant::ParticipantSelf(_))
                        | Some(tl::enums::ChannelParticipant::Admin(_))
                        | Some(tl::enums::ChannelParticipant::Creator(_))
                );
                app.set_member(&bot.name, -(1_000_000_000_000 + u.channel_id), member).await?;
            }
            return Ok(());
        }
        _ => return Ok(()),
    };
    let m = match msg {
        tl::enums::Message::Message(m) => m,
        tl::enums::Message::Service(s) => return on_service(app, bot, &s).await,
        tl::enums::Message::Empty(_) => return Ok(()),
    };
    let chat_id = marked_peer_id(&m.peer_id);
    if m.out {
        return Ok(());
    }
    let sender_id = match &m.from_id {
        Some(p) => marked_peer_id(p),
        None if chat_id > 0 => chat_id,
        None => 0,
    };
    // Ignore bots (including our other bots) and anonymous senders.
    let sender_peer = PeerId::from_bot_api_dialog_id(sender_id).and_then(|p| peers.get(p));
    let sender_is_bot =
        matches!(sender_peer, Some(Peer::User(u)) if u.is_bot()) || app.our_bot_ids().contains(&sender_id);
    if sender_id <= 0 || sender_is_bot {
        return Ok(());
    }
    app.set_member(&bot.name, chat_id, true).await?;
    remember_peers(app, chat_id, peers).await;

    let kind = classify(app, bot, chat_id, &m).await;
    let Some(kind) = kind else {
        return Ok(());
    };
    let mut payload = InboundPayload::default();
    payload.media = media_of(&m);
    if let Some(Peer::User(u)) = sender_peer {
        payload.sender_name =
            Some(format!("{} {}", u.first_name().unwrap_or(""), u.last_name().unwrap_or("")).trim().to_string());
        payload.sender_username = u.username().map(str::to_string);
    }
    if let Some(tl::enums::MessageReplyHeader::Header(h)) = &m.reply_to {
        payload.has_quote = h.quote;
    }
    let reply_to = match &m.reply_to {
        Some(tl::enums::MessageReplyHeader::Header(h)) => h.reply_to_msg_id,
        _ => None,
    };
    let (bot_name, text, date, msg_id, grouped) =
        (bot.name.clone(), m.message.clone(), m.date as i64, m.id, m.grouped_id);
    let payload_json = serde_json::to_string(&payload)?;
    let inserted: Option<i64> = app
        .db
        .call(move |c| {
            let n = c.execute(
                "INSERT OR IGNORE INTO inbound(bot, chat_id, msg_id, sender_id, date, text, reply_to_msg_id, grouped_id, payload, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'new', ?10)",
                params![bot_name, chat_id, msg_id, sender_id, date, text, reply_to, grouped, payload_json, now_ms()],
            )?;
            Ok(if n > 0 { Some(c.last_insert_rowid()) } else { None })
        })
        .await?;
    match inserted {
        Some(id) => {
            debug!(chat = chat_id, msg = msg_id, ?kind, inbound = id, "recorded inbound message");
            crate::handler::dispatch(app.clone(), id);
        }
        None => debug!(chat = chat_id, msg = msg_id, "duplicate update ignored"),
    }
    Ok(())
}

async fn on_service(app: &Arc<App>, bot: &Arc<BotConn>, s: &tl::types::MessageService) -> Result<()> {
    let chat_id = marked_peer_id(&s.peer_id);
    match &s.action {
        tl::enums::MessageAction::ChatAddUser(a) if a.users.contains(&bot.user_id) => {
            app.set_member(&bot.name, chat_id, true).await
        }
        tl::enums::MessageAction::ChatDeleteUser(a) if a.user_id == bot.user_id => {
            app.set_member(&bot.name, chat_id, false).await
        }
        tl::enums::MessageAction::ChatMigrateTo(a) => {
            info!(from = chat_id, to = a.channel_id, "group migrated to supergroup");
            app.set_member(&bot.name, -(1_000_000_000_000 + a.channel_id), true).await
        }
        _ => Ok(()),
    }
}

/// Cache names of users/chats seen in updates (for stats and admin listings).
async fn remember_peers(app: &App, chat_id: i64, peers: &PeerMap) {
    let mut users = Vec::new();
    let mut chats = Vec::new();
    for p in peers.iter_peers() {
        match p {
            Peer::User(u) => {
                if let Some(id) = u.id().bot_api_dialog_id() {
                    users.push((
                        id,
                        u.username().map(str::to_string),
                        u.first_name().map(str::to_string),
                        u.last_name().map(str::to_string),
                        u.is_bot(),
                    ));
                }
            }
            Peer::Group(g) => {
                if let Some(id) = g.id().bot_api_dialog_id() {
                    chats.push((id, "group", g.title().map(str::to_string), g.username().map(str::to_string)));
                }
            }
            Peer::Channel(ch) => {
                if let Some(id) = ch.id().bot_api_dialog_id() {
                    chats.push((id, "channel", Some(ch.title().to_string()), ch.username().map(str::to_string)));
                }
            }
            _ => {}
        }
    }
    if chat_id > 0 {
        chats.push((chat_id, "private", None, None));
    }
    let _ = app
        .db
        .call(move |c| {
            let now = now_ms();
            for (id, un, f, l, b) in users {
                c.execute(
                    "INSERT INTO users(user_id, username, first_name, last_name, is_bot, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(user_id) DO UPDATE SET username = excluded.username, first_name = excluded.first_name, last_name = excluded.last_name, updated_at = excluded.updated_at",
                    params![id, un, f, l, b, now],
                )?;
            }
            for (id, kind, title, un) in chats {
                c.execute(
                    "INSERT INTO chats(chat_id, kind, title, username, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(chat_id) DO UPDATE SET title = COALESCE(excluded.title, title), username = COALESCE(excluded.username, username), updated_at = excluded.updated_at",
                    params![id, kind, title, un, now],
                )?;
            }
            Ok(())
        })
        .await;
}

/// Parse `/cmd`, `/cmd@bot` at the very start of a message.
pub fn parse_command<'a>(text: &'a str, our_username: &str) -> Option<(String, &'a str, bool)> {
    let rest = text.strip_prefix('/')?;
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    let (head, args) = rest.split_at(end);
    let (name, target) = match head.split_once('@') {
        Some((n, t)) => (n, Some(t)),
        None => (head, None),
    };
    let name = name.to_ascii_lowercase();
    if !COMMANDS.contains(&name.as_str()) {
        return None;
    }
    let addressed = match target {
        Some(t) => {
            if !t.eq_ignore_ascii_case(our_username) {
                return None;
            }
            true
        }
        None => false,
    };
    Some((name, args.trim_start(), addressed))
}

async fn classify(app: &Arc<App>, bot: &Arc<BotConn>, chat_id: i64, m: &tl::types::Message) -> Option<Kind> {
    let text = m.message.as_str();
    if let Some((name, _, addressed)) = parse_command(text, &bot.username) {
        // Un-addressed commands in groups are answered by one bot only.
        if addressed || chat_id > 0 || app.primary_bot(chat_id, &bot.name) == bot.name {
            // `/m`, `/raw`, `/retry`, `/info` in replies are handled by the bot that owns the replied message.
            if matches!(name.as_str(), "m" | "raw" | "retry" | "info")
                && !addressed
                && let Some(rt) = reply_target(m)
            {
                crate::conv::turn_for_message(&app.db, &bot.name, chat_id, rt).await.ok().flatten()?;
            }
            return Some(Kind::Command(name));
        }
        return None;
    }
    if let Some(rt) = reply_target(m)
        && crate::conv::turn_for_message(&app.db, &bot.name, chat_id, rt).await.ok().flatten().is_some()
    {
        return Some(Kind::Reply);
    }
    let bot_cfg = app.bot_cfg(&bot.name)?;
    if let Ok(Some(t)) = omni_core::trigger::parse_trigger(text) {
        let strict = chat_id < 0;
        for s in &t.specs {
            if let Ok(Some(spec)) = app.resolve_spec(s, bot_cfg, strict).await
                && app.responsible_bot(chat_id, &spec.canonical(), &bot.name).as_deref() == Some(bot.name.as_str())
            {
                return Some(Kind::Trigger);
            }
        }
        if chat_id > 0 {
            return Some(Kind::Trigger); // let the handler explain what is wrong
        }
        return None;
    }
    if chat_id > 0 && m.media.is_none() {
        return Some(Kind::Hint);
    }
    None
}

pub fn reply_target(m: &tl::types::Message) -> Option<i32> {
    match &m.reply_to {
        Some(tl::enums::MessageReplyHeader::Header(h)) if h.reply_to_peer_id.is_none() => h.reply_to_msg_id,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        assert_eq!(parse_command("/raw", "omnibot"), Some(("raw".into(), "", false)));
        assert_eq!(parse_command("/m@OmniBot g$ hi", "omnibot"), Some(("m".into(), "g$ hi", true)));
        assert_eq!(parse_command("/m@other g$ hi", "omnibot"), None);
        assert_eq!(parse_command("/usr/bin is", "omnibot"), None);
        assert_eq!(parse_command("not a command", "omnibot"), None);
    }
}
