//! Turning recorded inbound messages into conversation turns and jobs.
//!
//! Handlers are idempotent: if the turns of an inbound message already exist
//! (crash after creating them), only the jobs are (re)scheduled.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use omni_core::Turn;
use omni_core::content::{Origin, Part, Role};
use omni_core::trigger::{self, SpecToken};
use tracing::{Instrument, info, info_span, warn};

use crate::access::{self, Access};
use crate::app::App;
use crate::conv::{self, Inbound, NewTurn, ResolvedSpec, TurnStatus};
use crate::intake::{Kind, parse_command};
use crate::jobs;
use crate::media;
use crate::reply;
use crate::tg::bot::BotConn;

pub fn dispatch(app: Arc<App>, inbound_id: i64) {
    // One handler per message: two would both find it new and create its turns twice.
    if !app.handling.lock().insert(inbound_id) {
        return;
    }
    tokio::spawn(async move {
        let span = info_span!("inbound", id = inbound_id);
        let res = handle(app.clone(), inbound_id).instrument(span).await;
        let (status, err) = match &res {
            Ok(()) => ("done", None),
            Err(e) => ("error", Some(format!("{e:#}"))),
        };
        if let Some(e) = &err {
            warn!(inbound = inbound_id, error = %e, "inbound handling failed");
        }
        let _ = conv::set_inbound_status(&app.db, inbound_id, status, err).await;
        // Only now: a later dispatch then sees the message handled.
        app.handling.lock().remove(&inbound_id);
    });
}

/// Re-dispatch inbound messages that were recorded but not handled (restart).
pub async fn recover(app: Arc<App>) -> Result<()> {
    for id in conv::pending_inbound(&app.db).await? {
        dispatch(app.clone(), id);
    }
    Ok(())
}

async fn handle(app: Arc<App>, id: i64) -> Result<()> {
    let ib = conv::get_inbound(&app.db, id).await?;
    if ib.status != "new" {
        return Ok(());
    }
    let bot = app.bot(&ib.bot).cloned().ok_or_else(|| anyhow!("bot {} not configured", ib.bot))?;
    let span = info_span!("msg", bot = %ib.bot, chat = ib.chat_id, user = ib.sender_id.unwrap_or(0), msg = ib.msg_id);
    async move {
        let user = ib.sender_id.unwrap_or(0);
        // Existing turns => crashed after creating them: just (re)start jobs.
        let existing = conv::turns_of_inbound(&app.db, ib.id).await?;
        if !existing.is_empty() {
            for (t, role) in existing {
                if role == Role::Assistant {
                    jobs::spawn_turn(app.clone(), t);
                }
            }
            return Ok(());
        }
        let kind = kind_of(&app, &bot, &ib).await;
        // Commands decide themselves (queries are answered everywhere).
        if let Some(Kind::Command(name)) = &kind {
            let args = parse_command(&ib.text, &bot.username).map(|(_, a, _)| a.to_string()).unwrap_or_default();
            return crate::commands::run(&app, &bot, &ib, name, &args).await;
        }
        // Conversations are paid requests.
        match access::check(&app, &bot.name, ib.chat_id, user).await? {
            Access::Allowed => {}
            Access::Banned => {
                if matches!(kind, Some(Kind::Trigger | Kind::Reply)) {
                    reply::notice(&app, &bot, ib.chat_id, ib.msg_id, &crate::commands::banned(ib.chat_id)).await;
                }
                return Ok(());
            }
            Access::NotAllowed => {
                info!("ignored (chat not allowed)");
                return Ok(());
            }
        }
        // An album arrives as one message per item. Handle it once, with the
        // first of its messages that was recorded (all album items of a reply
        // to an answer are; of a new conversation only those with a prefix).
        if let (Some(g), Some(Kind::Trigger | Kind::Reply)) = (ib.grouped_id, &kind) {
            tokio::time::sleep(ALBUM_WAIT).await;
            let leader = conv::album_leader(&app.db, &ib.bot, ib.chat_id, g).await?;
            if leader.is_some_and(|l| l != ib.msg_id) {
                info!(leader = ?leader, "album item handled with the album's first message");
                return Ok(());
            }
        }
        match kind {
            Some(Kind::Trigger) => new_conversation(&app, &bot, &ib).await,
            Some(Kind::Reply) => continue_conversation(&app, &bot, &ib, None).await,
            Some(Kind::Hint) => {
                reply::notice(&app, &bot, ib.chat_id, ib.msg_id, &crate::commands::hint(app.marker())).await;
                Ok(())
            }
            _ => Ok(()),
        }
    }
    .instrument(span)
    .await
}

async fn kind_of(app: &App, bot: &BotConn, ib: &Inbound) -> Option<Kind> {
    if let Some((name, _, _)) = parse_command(&ib.text, &bot.username) {
        return Some(Kind::Command(name));
    }
    if let Some(rt) = ib.reply_to_msg_id
        && conv::turn_for_message(&app.db, &bot.name, ib.chat_id, rt).await.ok().flatten().is_some()
    {
        return Some(Kind::Reply);
    }
    if trigger::find_marker(&ib.text, app.marker()).is_some() {
        return Some(Kind::Trigger);
    }
    if ib.chat_id > 0 {
        return Some(Kind::Hint);
    }
    None
}

/// Resolve the specs of a trigger for this bot. A prefix it does not know is
/// an error when the request is explicitly for this bot (`explicit`: private
/// chats, `/m`); in groups it is ignored, as by an independent bot (another
/// bot in the group may know it).
pub async fn resolve_specs(
    app: &App,
    bot: &BotConn,
    chat_id: i64,
    specs: &[SpecToken],
    explicit: bool,
) -> Result<Vec<ResolvedSpec>, String> {
    let cfg = app.bot_cfg(&bot.name).ok_or("bot not configured")?;
    if specs.len() > app.cfg.general.max_parallel_specs {
        return Err(format!("too many models in one request (max {})", app.cfg.general.max_parallel_specs));
    }
    let strict = chat_id < 0;
    let mut out = Vec::new();
    let mut unknown = Vec::new();
    for s in specs {
        let mut resolved = app.resolve_spec(s, cfg, chat_id, strict).await?;
        if resolved.is_none() && strict && app.plausible_model_id(cfg, s) {
            // Maybe a brand-new model: check a fresh catalog before giving up.
            app.refresh_catalog_if_older(std::time::Duration::from_secs(20)).await;
            resolved = app.resolve_spec(s, cfg, chat_id, strict).await?;
        }
        match resolved {
            Some(spec) => out.push(spec),
            None => unknown.push(format!("`{}{}`", s.raw, app.marker())),
        }
    }
    if !unknown.is_empty() && explicit {
        let ids = crate::commands::direct_ids(app)
            .map(|ids| format!("; the id of a model this bot has works too ({ids})"))
            .unwrap_or_default();
        return Err(format!("unknown prefix {}: /models lists the prefixes{ids}", unknown.join(", ")));
    }
    Ok(out)
}

/// How long to wait for the other items of an album (they arrive together).
const ALBUM_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);

/// What a message brings besides its own text: media and positions (its own,
/// the rest of its album's, and those of the replied-to message when that is
/// not a bot message, like the old bot), and the captions of its album's
/// other items.
struct Attached {
    parts: Vec<Part>,
    captions: Vec<String>,
}

/// Files one message may bring (its album and a replied-to album included).
const MAX_ATTACHMENTS: usize = 40;

/// Limits on what one message brings, far above what real messages need and
/// low enough that a crafted one cannot exhaust memory: at most
/// [`MAX_ATTACHMENTS`] files, 4 × `max_file_mb` in all, and that size times the
/// models it goes to at most 10 × `max_file_mb` (each model's request holds its
/// own copy). Returns (total, fan-out) in bytes.
fn attachment_limits(app: &App) -> (u64, u64) {
    let file = app.cfg.general_max_file_bytes();
    (file.saturating_mul(4), file.saturating_mul(10))
}

fn mb(b: u64) -> String {
    format!("{:.0} MB", b as f64 / 1e6)
}

/// `Err(notice)` if `parts` sent to `models` models would exceed the fan-out limit.
fn check_fan_out(app: &App, parts: &[Part], models: usize) -> Result<(), String> {
    let bytes: u64 = parts
        .iter()
        .map(|p| match p {
            Part::Image { blob, .. } | Part::File { blob, .. } => blob.size,
            _ => 0,
        })
        .sum();
    let (_, fan_out) = attachment_limits(app);
    if models > 1 && bytes.saturating_mul(models as u64) > fan_out {
        return Err(format!(
            "⚠️ too many models for files this large: {models} × {} is above {}; ask fewer models at once",
            mb(bytes),
            mb(fan_out)
        ));
    }
    Ok(())
}

/// Collect the media of a message (album-aware) as blobs/parts.
async fn media_parts(app: &App, bot: &BotConn, ib: &Inbound) -> Result<Attached> {
    let max = app.cfg.general_max_file_bytes();
    let mut refs = ib.payload.media.clone();
    let mut locations = ib.payload.locations.clone();
    let mut cards = ib.payload.cards.clone();
    let mut captions = Vec::new();
    if let Some(g) = ib.grouped_id
        && let Ok(items) = media::album(bot, ib.chat_id, ib.msg_id, g).await
        && !items.is_empty()
    {
        refs = items.iter().flat_map(|i| i.media.clone()).collect();
        captions = items
            .iter()
            .filter(|i| i.msg_id != ib.msg_id && !i.text.trim().is_empty())
            .map(|i| i.text.clone())
            .collect();
    }
    // Media and positions of a replied-to (non-bot) message, like the old bot.
    if let Some(rt) = ib.reply_to_msg_id
        && conv::turn_for_message(&app.db, &bot.name, ib.chat_id, rt).await?.is_none()
        && let Ok(msgs) = bot.get_messages(ib.chat_id, &[rt]).await
    {
        for m in msgs {
            if let grammers_tl_types::enums::Message::Message(m) = m {
                let mut r = media::media_of(&m);
                if let Some(g) = m.grouped_id
                    && let Ok(items) = media::album(bot, ib.chat_id, m.id, g).await
                    && !items.is_empty()
                {
                    r = items.into_iter().flat_map(|i| i.media).collect();
                }
                r.extend(refs);
                refs = r;
                let mut l = media::locations_of(&m);
                l.extend(locations);
                locations = l;
                let mut c = media::cards_of(&m);
                c.extend(cards);
                cards = c;
            }
        }
    }
    let (max_total, _) = attachment_limits(app);
    if refs.len() > MAX_ATTACHMENTS {
        anyhow::bail!("this message brings {} files; at most {MAX_ATTACHMENTS} at once", refs.len());
    }
    let declared: u64 = refs.iter().filter_map(|r| r.size).sum();
    if declared > max_total {
        anyhow::bail!("the files of this message are {} in all; at most {} at once", mb(declared), mb(max_total));
    }
    let mut parts = Vec::new();
    let mut total = 0u64;
    // Every kind of Telegram media: photos, files, voice and video messages,
    // GIFs, stickers (an animated one as its still preview).
    for r in &refs {
        let blob = media::fetch(&app.db, &app.blobs, bot, ib.chat_id, r, max)
            .await
            .with_context(|| format!("cannot use {}", r.name()))?;
        total += blob.size;
        if total > max_total {
            anyhow::bail!("the files of this message are more than {} in all", mb(max_total));
        }
        parts.push(match r.kind {
            media::MediaKind::Photo => {
                Part::Image { blob, filename: None, generated: false, dims: None, thought: false }
            }
            media::MediaKind::Document if r.mime.starts_with("image/") => {
                Part::Image { blob, filename: r.filename.clone(), generated: false, dims: None, thought: false }
            }
            media::MediaKind::Document => Part::File { blob, filename: r.filename.clone(), generated: false },
        });
    }
    parts.extend(locations.iter().map(media::LocationRef::part));
    parts.extend(cards.into_iter().map(Part::text));
    Ok(Attached { parts, captions })
}

/// Build user turns from message text (role markers -> synthetic history).
fn user_turns(text: &str, media: Vec<Part>) -> Result<Vec<Turn>, String> {
    let segments = trigger::parse_history(text);
    let mut turns: Vec<Turn> = match segments {
        None => vec![Turn {
            role: Role::User,
            origin: Origin::Live,
            parts: if text.is_empty() { vec![] } else { vec![Part::text(trigger::unescape_markers(text))] },
            native: None,
        }],
        Some(segs) => segs
            .into_iter()
            .map(|s| Turn {
                role: s.role,
                origin: Origin::Synthetic,
                parts: if s.text.is_empty() { vec![] } else { vec![Part::text(s.text)] },
                native: None,
            })
            .collect(),
    };
    // Media belongs to the last user turn.
    if !media.is_empty() {
        let idx = turns
            .iter()
            .rposition(|t| t.role == Role::User)
            .ok_or("history has no user turn to attach the media to")?;
        let t = &mut turns[idx];
        let mut parts = media;
        parts.append(&mut t.parts);
        t.parts = parts;
        t.origin = Origin::Live;
    }
    if let Some(last_user) = turns.iter_mut().rev().find(|t| t.role == Role::User) {
        last_user.origin = Origin::Live;
        // `@https://youtube.com/...` attaches the video by URL (fetched by the provider, e.g. Gemini),
        // like the old bot. The text itself is sent unchanged.
        let urls: Vec<String> = last_user
            .parts
            .iter()
            .filter_map(|p| if let Part::Text { text, .. } = p { Some(youtube_refs(text)) } else { None })
            .flatten()
            .collect();
        for url in urls {
            last_user.parts.insert(0, Part::RemoteMedia { url, kind: omni_core::content::RemoteMediaKind::YouTube });
        }
    }
    if turns.iter().all(|t| t.parts.is_empty()) {
        return Err("empty message".into());
    }
    Ok(turns)
}

/// `@https://www.youtube.com/watch?v=...` / `@https://youtu.be/...` references in text.
pub fn youtube_refs(text: &str) -> Vec<String> {
    let re =
        regex::Regex::new(r"@(https://(?:(?:www\.|m\.)?youtube\.com|youtu\.be)/[A-Za-z0-9./?=&%_~#:+-]+)").unwrap();
    re.captures_iter(text).map(|c| c[1].to_string()).collect()
}

/// Whether a reply to a message that is not one of this bot's answers brings
/// something to attach (media, an album, a location, a poll…); `None` when
/// the message is no such reply. The old bot's rule: a reply starts a
/// conversation only to bring what it replies to; a reply to anything else
/// (someone's text, another bot's answer) is ignored, prefix or not.
async fn reply_brings(app: &App, bot: &BotConn, ib: &Inbound) -> Result<Option<bool>> {
    let Some(rt) = ib.reply_to_msg_id else { return Ok(None) };
    if conv::turn_for_message(&app.db, &bot.name, ib.chat_id, rt).await?.is_some() {
        return Ok(None);
    }
    // When the message cannot be read, as before: the request goes ahead.
    let Ok(msgs) = bot.get_messages(ib.chat_id, &[rt]).await else { return Ok(Some(true)) };
    Ok(Some(msgs.iter().any(|m| match m {
        grammers_tl_types::enums::Message::Message(m) => {
            m.grouped_id.is_some()
                || !media::media_of(m).is_empty()
                || !media::locations_of(m).is_empty()
                || !media::cards_of(m).is_empty()
        }
        _ => false,
    })))
}

async fn new_conversation(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<()> {
    if reply_brings(app, bot, ib).await? == Some(false) {
        if ib.chat_id > 0 {
            reply::notice(
                app,
                bot,
                ib.chat_id,
                ib.msg_id,
                "⚠️ a reply only continues one of my answers, or brings me the photo, file or location it replies \
                 to; to start a conversation, send the message without replying",
            )
            .await;
        }
        return Ok(());
    }
    let Ok(Some(t)) = trigger::parse_trigger(&ib.text, app.marker()).map_err(|e| e.to_string()).inspect_err(|e| {
        let e = e.clone();
        let (app, bot, chat, msg) = (app.clone(), bot.clone(), ib.chat_id, ib.msg_id);
        tokio::spawn(async move { reply::notice(&app, &bot, chat, msg, &format!("⚠️ {e}")).await });
    }) else {
        return Ok(());
    };
    let specs = match resolve_specs(app, bot, ib.chat_id, &t.specs, ib.chat_id > 0).await {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => return Ok(()),
        Err(e) => {
            reply::notice(app, bot, ib.chat_id, ib.msg_id, &format!("⚠️ {e}")).await;
            return Ok(());
        }
    };
    let attached = match media_parts(app, bot, ib).await {
        Ok(m) => m,
        Err(e) => {
            reply::notice(app, bot, ib.chat_id, ib.msg_id, &format!("⚠️ {e:#}")).await;
            return Ok(());
        }
    };
    if let Err(msg) = check_fan_out(app, &attached.parts, specs.len()) {
        reply::notice(app, bot, ib.chat_id, ib.msg_id, &msg).await;
        return Ok(());
    }
    let turns = match user_turns(&t.text, attached.parts).map(|mut ts| {
        // Captions of the album's other items follow the message text.
        if let Some(last) = ts.iter_mut().rev().find(|t| t.role == Role::User) {
            last.parts.extend(attached.captions.iter().map(|c| Part::text(c.clone())));
        }
        ts
    }) {
        Ok(t) => t,
        Err(e) => {
            reply::notice(app, bot, ib.chat_id, ib.msg_id, &format!("⚠️ {e}")).await;
            return Ok(());
        }
    };
    // A chat prefix brings its own system prompt: the message cannot add another.
    let has_system = |ts: &[Turn]| ts.iter().any(|t| t.role == Role::System);
    if has_system(&turns) && specs.iter().any(|s| s.template.as_ref().is_some_and(|t| has_system(t))) {
        let msg = "⚠️ this prefix already has a system prompt; leave out ::system";
        reply::notice(app, bot, ib.chat_id, ib.msg_id, msg).await;
        return Ok(());
    }
    create_and_start(app, bot, ib, None, turns, specs).await
}

/// A reply to a bot message (or `/m` with new specs).
pub async fn continue_conversation(
    app: &Arc<App>,
    bot: &Arc<BotConn>,
    ib: &Inbound,
    new_specs: Option<(Vec<ResolvedSpec>, String)>,
) -> Result<()> {
    if ib.payload.has_quote && new_specs.is_none() {
        info!("reply with quote ignored");
        return Ok(());
    }
    let rt = ib.reply_to_msg_id.context("not a reply")?;
    let Some((turn_id, _)) = conv::turn_for_message(&app.db, &bot.name, ib.chat_id, rt).await? else {
        return Ok(());
    };
    let parent = conv::get_turn(&app.db, turn_id).await?;
    if parent.turn.role == Role::Assistant && parent.status == TurnStatus::Failed {
        reply::notice(
            app,
            bot,
            ib.chat_id,
            ib.msg_id,
            "⚠️ That message failed to generate, so there is nothing to continue from. Press 🔁 Retry under it (or reply /retry to it) to try again, or reply to an earlier message.",
        )
        .await;
        return Ok(());
    }
    let (specs, text) = match new_specs {
        Some(s) => s,
        None => {
            let spec = parent.spec.clone().context("replied turn has no model")?;
            // A leading backslash escapes commands/markers: everything after it is sent verbatim.
            let text = ib.text.strip_prefix('\\').unwrap_or(&ib.text).to_string();
            (vec![spec], text)
        }
    };
    let attached = match media_parts(app, bot, ib).await {
        Ok(m) => m,
        Err(e) => {
            reply::notice(app, bot, ib.chat_id, ib.msg_id, &format!("⚠️ {e:#}")).await;
            return Ok(());
        }
    };
    if let Err(msg) = check_fan_out(app, &attached.parts, specs.len()) {
        reply::notice(app, bot, ib.chat_id, ib.msg_id, &msg).await;
        return Ok(());
    }
    let mut parts = attached.parts;
    if !text.is_empty() {
        parts.push(Part::text(text));
    }
    parts.extend(attached.captions.into_iter().map(Part::text));
    if parts.is_empty() {
        reply::notice(app, bot, ib.chat_id, ib.msg_id, "⚠️ empty message").await;
        return Ok(());
    }
    let turns = vec![Turn { role: Role::User, origin: Origin::Live, parts, native: None }];
    create_and_start(app, bot, ib, Some(turn_id), turns, specs).await?;
    // It continues from an answer that is not finished: it waits for it (jobs).
    if matches!(parent.status, TurnStatus::Pending | TurnStatus::Running) {
        let msg = "⏳ queued: the answer this continues is still being generated; this one starts when it is finished";
        reply::notice(app, bot, ib.chat_id, ib.msg_id, msg).await;
    }
    Ok(())
}

async fn create_and_start(
    app: &Arc<App>,
    bot: &Arc<BotConn>,
    ib: &Inbound,
    parent: Option<i64>,
    turns: Vec<Turn>,
    specs: Vec<ResolvedSpec>,
) -> Result<()> {
    let (bot_name, chat, user, inbound) = (bot.name.clone(), ib.chat_id, ib.sender_id, ib.id);
    let assistant_ids = app
        .db
        .call(move |c| {
            let tx = c.transaction()?;
            let new = |parent: Option<i64>, turn: Turn, spec: Option<ResolvedSpec>, status: TurnStatus| NewTurn {
                parent_id: parent,
                turn,
                spec,
                status,
                bot: Some(bot_name.clone()),
                chat_id: Some(chat),
                user_id: user,
                inbound_id: Some(inbound),
            };
            // Insert turns one after the other below `parent`; the last one's id.
            let chain = |tx: &rusqlite::Transaction, parent: Option<i64>, ts: &mut dyn Iterator<Item = &Turn>| {
                let mut prev = parent;
                for t in ts {
                    prev = Some(conv::insert_turn_sync(tx, &new(prev, t.clone(), None, TurnStatus::Done))?);
                }
                prev.context("no user turn")
            };
            // Only bot messages are mapped to turns: replies to the user's own message
            // are not continuations (same as the old bot).
            let mut shared: Option<i64> = None;
            let mut ids = Vec::new();
            for s in &specs {
                let last = match &s.template {
                    // The message's turns, shared by every model without a chat prefix.
                    None => match shared {
                        Some(id) => id,
                        None => *shared.insert(chain(&tx, parent, &mut turns.iter())?),
                    },
                    // A chat prefix: its system prompt and history first, in its own chain.
                    Some(tpl) => chain(&tx, parent, &mut tpl.iter().chain(turns.iter()))?,
                };
                let answer = Turn { role: Role::Assistant, origin: Origin::Live, parts: vec![], native: None };
                ids.push(conv::insert_turn_sync(&tx, &new(Some(last), answer, Some(s.clone()), TurnStatus::Pending))?);
            }
            tx.commit()?;
            Ok(ids)
        })
        .await?;
    info!(turns = ?assistant_ids, "created assistant turns");
    for id in assistant_ids {
        jobs::spawn_turn(app.clone(), id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_references() {
        assert_eq!(
            youtube_refs(
                "see @https://www.youtube.com/watch?v=abc_12-3&t=5 and @https://youtu.be/xyz but not https://youtube.com/no"
            ),
            vec!["https://www.youtube.com/watch?v=abc_12-3&t=5".to_string(), "https://youtu.be/xyz".to_string()]
        );
    }

    #[test]
    fn history_turns_and_media_placement() {
        let img = Part::Image {
            blob: omni_core::BlobRef::for_bytes(b"x", "image/jpeg"),
            filename: None,
            generated: false,
            dims: None,
            thought: false,
        };
        let t = user_turns("::system\nbe brief\n::user\nwhat is this?\n::assistant\nIt is", vec![img.clone()]).unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].role, Role::System);
        assert_eq!(t[1].role, Role::User);
        assert!(matches!(t[1].parts[0], Part::Image { .. }));
        assert_eq!(t[2].role, Role::Assistant);
        assert_eq!(t[2].origin, Origin::Synthetic);
        assert!(user_turns("", vec![]).is_err());
        let t = user_turns("plain question", vec![]).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].origin, Origin::Live);
    }
}
